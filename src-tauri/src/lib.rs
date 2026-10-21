use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::UNIX_EPOCH,
};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::DialogExt;
use walkdir::WalkDir;

const SNAPSHOT_MAGIC: &[u8; 8] = b"TDSNAP01";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotInfo {
    id: String,
    name: String,
    root_path: String,
    created_at: String,
    file_count: u64,
    directory_count: u64,
    size_bytes: u64,
    category: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileRecord {
    relative_path: String,
    size_bytes: u64,
    modified_unix_ms: Option<u128>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotData {
    info: SnapshotInfo,
    records: Vec<FileRecord>,
    #[serde(default)]
    directories: Vec<String>,
    skipped_entries: u64,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppSettings {
    snapshot_directory: Option<String>,
    #[serde(default)]
    snapshot_folders: Vec<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanProgress {
    scanned: u64,
    total: u64,
    percent: u8,
    phase: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiffFile {
    path: String,
    size_bytes: u64,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_size_bytes: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ComparisonResult {
    old_snapshot: SnapshotInfo,
    new_snapshot: SnapshotInfo,
    added_bytes: u64,
    removed_bytes: u64,
    added_files: Vec<DiffFile>,
    removed_files: Vec<DiffFile>,
    changed_files: Vec<DiffFile>,
}

fn normalize_relative_path(path: &str) -> String {
    path.replace('/', "\\")
        .trim_matches('\\')
        .to_lowercase()
}

fn diff_snapshot_records(old: &[FileRecord], new: &[FileRecord]) -> (Vec<DiffFile>, Vec<DiffFile>, Vec<DiffFile>, u64, u64) {
    let old_records = old.iter()
        .map(|record| (normalize_relative_path(&record.relative_path), record))
        .collect::<HashMap<_, _>>();
    let new_records = new.iter()
        .map(|record| (normalize_relative_path(&record.relative_path), record))
        .collect::<HashMap<_, _>>();
    let mut added_files = Vec::new();
    let mut removed_files = Vec::new();
    let mut changed_files = Vec::new();
    let mut added_bytes = 0_u64;
    let mut removed_bytes = 0_u64;

    // Status is based only on path presence. Size is used only for the summary amount.
    for (key, record) in &new_records {
        if !old_records.contains_key(key) {
            added_bytes = added_bytes.saturating_add(record.size_bytes);
            added_files.push(DiffFile { path: record.relative_path.clone(), size_bytes: record.size_bytes, status: "added", previous_size_bytes: None, current_size_bytes: Some(record.size_bytes) });
        }
    }
    for (key, record) in &old_records {
        if !new_records.contains_key(key) {
            removed_bytes = removed_bytes.saturating_add(record.size_bytes);
            removed_files.push(DiffFile { path: record.relative_path.clone(), size_bytes: record.size_bytes, status: "removed", previous_size_bytes: Some(record.size_bytes), current_size_bytes: None });
        }
    }
    for (key, old_record) in &old_records {
        let Some(new_record) = new_records.get(key) else { continue; };
        if old_record.size_bytes != new_record.size_bytes {
            changed_files.push(DiffFile {
                path: new_record.relative_path.clone(),
                size_bytes: new_record.size_bytes,
                status: "changed",
                previous_size_bytes: Some(old_record.size_bytes),
                current_size_bytes: Some(new_record.size_bytes),
            });
        }
    }
    added_files.sort_unstable_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));
    removed_files.sort_unstable_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));
    changed_files.sort_unstable_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));
    (added_files, removed_files, changed_files, added_bytes, removed_bytes)
}

fn default_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app.path().app_data_dir().map_err(|error| error.to_string())?.join("snapshots"))
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app.path().app_data_dir().map_err(|error| error.to_string())?.join("settings.json"))
}

fn read_settings(app: &AppHandle) -> AppSettings {
    settings_path(app)
        .ok()
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn clean_windows_path(path: &str) -> String {
    #[cfg(windows)]
    let path = path.replace('/', "\\");
    #[cfg(not(windows))]
    let path = path.to_string();

    let uppercase = path.to_ascii_uppercase();
    if uppercase.starts_with("\\\\?\\UNC\\") {
        format!("\\\\{}", &path[8..])
    } else if uppercase.starts_with("\\\\?\\") && path.as_bytes().get(5) == Some(&b':') {
        path[4..].to_string()
    } else {
        path
    }
}

fn clean_path(path: &Path) -> String {
    clean_windows_path(&path.to_string_lossy())
}

fn log_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app.path().app_data_dir().map_err(|error| error.to_string())?.join("tracedisk.log"))
}

fn append_log(app: &AppHandle, message: impl AsRef<str>) {
    let Ok(path) = log_path(app) else { return; };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} {}", Local::now().to_rfc3339(), message.as_ref());
    }
}

#[tauri::command]
fn get_app_log(app: AppHandle) -> Result<String, String> {
    let path = log_path(&app)?;
    let content = fs::read_to_string(path).unwrap_or_default();
    let lines = content.lines().collect::<Vec<_>>();
    Ok(lines.into_iter().rev().take(200).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"))
}

#[tauri::command]
fn clear_app_log(app: AppHandle) -> Result<(), String> {
    let path = log_path(&app)?;
    if path.exists() {
        fs::remove_file(path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn emit_scan_progress(app: &AppHandle, scanned: u64, total: u64) {
    let percent = if total == 0 { 0 } else { scanned.saturating_mul(100).checked_div(total).unwrap_or(100).min(100) as u8 };
    let _ = app.emit("scan-progress", ScanProgress { scanned, total, percent, phase: "scanning" });
}

fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let default = default_data_dir(app)?;
    let path = read_settings(app)
        .snapshot_directory
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or(default);
    fs::create_dir_all(&path).map_err(|error| format!("Cannot create snapshot folder: {error}"))?;
    Ok(path)
}

#[tauri::command]
fn get_snapshot_directory(app: AppHandle) -> Result<String, String> {
    Ok(clean_path(&data_dir(&app)?))
}

#[tauri::command]
fn get_app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

fn write_settings(app: &AppHandle, settings: &AppSettings) -> Result<(), String> {
    let path = settings_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(settings).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    replace_file(&temp, &path)
}

#[tauri::command]
fn set_snapshot_directory(app: AppHandle, path: String) -> Result<String, String> {
    let selected = fs::canonicalize(PathBuf::from(path.trim()))
        .map_err(|error| format!("Cannot use this folder: {error}"))?;
    if !selected.is_dir() {
        return Err("The selected path is not a folder".into());
    }
    fs::create_dir_all(&selected).map_err(|error| error.to_string())?;
    let selected_path = clean_path(&selected);
    let mut settings = read_settings(&app);
    settings.snapshot_directory = Some(selected_path.clone());
    write_settings(&app, &settings)?;
    append_log(&app, format!("snapshot_directory_changed path={selected_path}"));
    Ok(selected_path)
}

#[tauri::command]
fn reset_snapshot_directory(app: AppHandle) -> Result<String, String> {
    let default = default_data_dir(&app)?;
    fs::create_dir_all(&default).map_err(|error| error.to_string())?;
    let mut settings = read_settings(&app);
    settings.snapshot_directory = None;
    write_settings(&app, &settings)?;
    let default_path = clean_path(&default);
    append_log(&app, format!("snapshot_directory_reset path={default_path}"));
    Ok(default_path)
}

#[tauri::command]
fn list_snapshot_folders(app: AppHandle) -> Result<Vec<String>, String> {
    let mut folders = read_settings(&app).snapshot_folders;
    for snapshot in list_snapshots(app)? {
        if snapshot.category != "未分类" && !folders.iter().any(|name| name.eq_ignore_ascii_case(&snapshot.category)) {
            folders.push(snapshot.category);
        }
    }
    folders.sort_by_key(|name| name.to_lowercase());
    Ok(folders)
}

#[tauri::command]
fn create_snapshot_folder(app: AppHandle, name: String) -> Result<Vec<String>, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().any(|character| matches!(character, '\\' | '/')) || name == "未分类" {
        return Err("Enter a valid folder name".into());
    }
    let mut settings = read_settings(&app);
    if settings.snapshot_folders.iter().any(|folder| folder.eq_ignore_ascii_case(name)) {
        return Err("A folder with this name already exists".into());
    }
    settings.snapshot_folders.push(name.to_string());
    write_settings(&app, &settings)?;
    append_log(&app, format!("snapshot_folder_created name={name}"));
    list_snapshot_folders(app)
}

#[tauri::command]
fn delete_snapshot_folder(app: AppHandle, name: String) -> Result<Vec<String>, String> {
    let name = name.trim();
    if name.is_empty() || name == "未分类" {
        return Err("The root folder cannot be deleted".into());
    }
    let mut settings = read_settings(&app);
    settings.snapshot_folders.retain(|folder| !folder.eq_ignore_ascii_case(name));
    write_settings(&app, &settings)?;

    for snapshot in list_snapshots(app.clone())? {
        if !snapshot.category.eq_ignore_ascii_case(name) {
            continue;
        }
        let path = metadata_path(&app, &snapshot.id)?;
        let mut info: SnapshotInfo = serde_json::from_slice(
            &fs::read(&path).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        info.category = "未分类".into();
        write_info(&path, &info)?;
    }
    append_log(&app, format!("snapshot_folder_deleted name={name}"));
    list_snapshot_folders(app)
}

fn snapshot_path(app: &AppHandle, id: &str) -> Result<PathBuf, String> {
    validate_snapshot_id(id)?;
    Ok(data_dir(app)?.join(format!("{id}.tdsnap")))
}

fn metadata_path(app: &AppHandle, id: &str) -> Result<PathBuf, String> {
    validate_snapshot_id(id)?;
    Ok(data_dir(app)?.join(format!("{id}.json")))
}

fn validate_snapshot_id(id: &str) -> Result<(), String> {
    if !id.is_empty() && id.len() <= 80 && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) {
        Ok(())
    } else {
        Err("Invalid snapshot id".into())
    }
}

fn new_snapshot_id(app: &AppHandle, created: &DateTime<Local>) -> Result<String, String> {
    let base = created.format("%Y-%m-%d_%H-%M-%S-%3f").to_string();
    let mut id = base.clone();
    let mut suffix = 1_u32;
    while snapshot_path(app, &id)?.exists() {
        id = format!("{base}-{suffix:02}");
        suffix += 1;
    }
    Ok(id)
}

fn write_snapshot(path: &Path, data: &SnapshotData) -> Result<(), String> {
    let payload = serde_json::to_vec(data).map_err(|error| error.to_string())?;
    let compressed = zstd::stream::encode_all(payload.as_slice(), 3).map_err(|error| error.to_string())?;
    let mut bytes = Vec::with_capacity(SNAPSHOT_MAGIC.len() + compressed.len());
    bytes.extend_from_slice(SNAPSHOT_MAGIC);
    bytes.extend_from_slice(&compressed);

    let temp = path.with_extension("tdsnap.tmp");
    fs::write(&temp, bytes).map_err(|error| format!("Cannot write snapshot: {error}"))?;
    fs::rename(&temp, path).map_err(|error| format!("Cannot finish snapshot: {error}"))
}

fn read_snapshot(path: &Path) -> Result<SnapshotData, String> {
    let bytes = fs::read(path).map_err(|error| format!("Cannot read snapshot: {error}"))?;
    if bytes.len() <= SNAPSHOT_MAGIC.len() || &bytes[..SNAPSHOT_MAGIC.len()] != SNAPSHOT_MAGIC {
        return Err("This snapshot is invalid or uses an unsupported format".into());
    }
    let mut decoder = zstd::stream::read::Decoder::new(&bytes[SNAPSHOT_MAGIC.len()..])
        .map_err(|error| format!("Cannot decompress snapshot: {error}"))?;
    let mut data: SnapshotData = serde_json::from_reader(&mut decoder)
        .map_err(|error| format!("Snapshot data is invalid: {error}"))?;
    data.info.root_path = clean_windows_path(&data.info.root_path);
    Ok(data)
}

fn write_info(path: &Path, info: &SnapshotInfo) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(info).map_err(|error| error.to_string())?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, bytes).map_err(|error| error.to_string())?;
    replace_file(&temp, path)
}

fn replace_file(temp: &Path, target: &Path) -> Result<(), String> {
    if !target.exists() {
        return fs::rename(temp, target).map_err(|error| error.to_string());
    }
    let backup = target.with_extension("json.bak");
    fs::rename(target, &backup).map_err(|error| error.to_string())?;
    if let Err(error) = fs::rename(temp, target) {
        let _ = fs::rename(&backup, target);
        return Err(error.to_string());
    }
    let _ = fs::remove_file(backup);
    Ok(())
}

fn create_snapshot_inner(app: AppHandle, root_path: String, name: Option<String>) -> Result<SnapshotInfo, String> {
    let root = PathBuf::from(&root_path);
    if !root.is_dir() {
        return Err("The selected path is not a folder".into());
    }
    let root_path = clean_path(&fs::canonicalize(&root).unwrap_or(root));
    let folder_name = Path::new(&root_path)
        .file_name()
        .and_then(|part| part.to_str())
        .filter(|part| !part.is_empty())
        .unwrap_or(&root_path)
        .to_owned();
    let created: DateTime<Local> = Local::now();
    let id = new_snapshot_id(&app, &created)?;
    append_log(&app, format!("scan_started root={root_path}"));
    let mut records = Vec::new();
    let mut directories = Vec::new();
    let mut directory_count = 0_u64;
    let mut skipped_entries = 0_u64;
    let mut scanned_entries = 0_u64;
    // One traversal avoids the expensive pre-count pass on large disks.
    emit_scan_progress(&app, 0, 0);

    for result in WalkDir::new(&root_path).follow_links(false).into_iter().skip(1) {
        scanned_entries += 1;
        let entry = match result {
            Ok(entry) => entry,
            Err(_) => {
                skipped_entries += 1;
                continue;
            }
        };
        if scanned_entries % 64 == 0 {
            emit_scan_progress(&app, scanned_entries, 0);
        }
        if entry.file_type().is_symlink() {
            continue;
        }
        if entry.file_type().is_dir() {
            directory_count += 1;
            if let Ok(path) = entry.path().strip_prefix(&root_path) {
                directories.push(path.to_string_lossy().replace('/', "\\"));
            }
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                skipped_entries += 1;
                continue;
            }
        };
        let relative_path = match entry.path().strip_prefix(&root_path) {
            Ok(path) => path.to_string_lossy().replace('/', "\\"),
            Err(_) => continue,
        };
        let modified_unix_ms = metadata.modified().ok().and_then(|time| time.duration_since(UNIX_EPOCH).ok()).map(|duration| duration.as_millis());
        records.push(FileRecord { relative_path, size_bytes: metadata.len(), modified_unix_ms });
    }

    records.sort_unstable_by(|a, b| a.relative_path.to_lowercase().cmp(&b.relative_path.to_lowercase()));
    directories.sort_unstable_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    let snapshot_path = snapshot_path(&app, &id)?;
    let mut info = SnapshotInfo {
        id: id.clone(),
        name: name.unwrap_or_default().trim().to_string(),
        root_path,
        created_at: created.to_rfc3339(),
        file_count: records.len() as u64,
        directory_count,
        size_bytes: 0,
        category: "未分类".into(),
    };
    if info.name.is_empty() {
        info.name = format!("{} · {}", folder_name, created.format("%Y-%m-%d %H:%M:%S"));
    }
    let data = SnapshotData { info: info.clone(), records, directories, skipped_entries };
    write_snapshot(&snapshot_path, &data)?;
    info.size_bytes = fs::metadata(&snapshot_path).map(|metadata| metadata.len()).unwrap_or(0);
    if let Err(error) = write_info(&metadata_path(&app, &id)?, &info) {
        let _ = fs::remove_file(snapshot_path);
        return Err(error);
    }
    emit_scan_progress(&app, scanned_entries, scanned_entries);
    append_log(&app, format!("scan_completed id={} records={} skipped={}", info.id, info.file_count, skipped_entries));
    Ok(info)
}

#[tauri::command]
fn list_snapshots(app: AppHandle) -> Result<Vec<SnapshotInfo>, String> {
    let directory = data_dir(&app)?;
    let mut snapshots = Vec::new();
    for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") { continue; }
        if let Ok(bytes) = fs::read(&path) {
            if let Ok(mut info) = serde_json::from_slice::<SnapshotInfo>(&bytes) {
                info.root_path = clean_windows_path(&info.root_path);
                if validate_snapshot_id(&info.id).is_ok() && snapshot_path(&app, &info.id)?.is_file() {
                    info.size_bytes = fs::metadata(snapshot_path(&app, &info.id)?).map(|metadata| metadata.len()).unwrap_or(info.size_bytes);
                    snapshots.push(info);
                }
            }
        }
    }
    snapshots.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(snapshots)
}

#[tauri::command]
async fn create_snapshot(app: AppHandle, root_path: String, name: Option<String>) -> Result<SnapshotInfo, String> {
    tauri::async_runtime::spawn_blocking(move || create_snapshot_inner(app, root_path, name))
        .await.map_err(|error| error.to_string())?
}

#[tauri::command]
async fn get_snapshot_contents(app: AppHandle, id: String) -> Result<SnapshotData, String> {
    tauri::async_runtime::spawn_blocking(move || read_snapshot(&snapshot_path(&app, &id)?))
        .await.map_err(|error| error.to_string())?
}

#[tauri::command]
async fn compare_snapshots(app: AppHandle, old_id: String, new_id: String) -> Result<ComparisonResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        if old_id == new_id { return Err("Choose two different snapshots".into()); }
        let old_data = read_snapshot(&snapshot_path(&app, &old_id)?)?;
        let new_data = read_snapshot(&snapshot_path(&app, &new_id)?)?;
        let old_created = DateTime::parse_from_rfc3339(&old_data.info.created_at)
            .map_err(|error| format!("Invalid baseline snapshot date: {error}"))?;
        let new_created = DateTime::parse_from_rfc3339(&new_data.info.created_at)
            .map_err(|error| format!("Invalid current snapshot date: {error}"))?;
        if old_created >= new_created {
            return Err("The baseline snapshot must be earlier than the current snapshot".into());
        }
        if !old_data.info.root_path.eq_ignore_ascii_case(&new_data.info.root_path) {
            return Err("Snapshots must use the same root folder".into());
        }
        let (added_files, removed_files, changed_files, added_bytes, removed_bytes) = diff_snapshot_records(&old_data.records, &new_data.records);
        Ok(ComparisonResult { old_snapshot: old_data.info, new_snapshot: new_data.info, added_bytes, removed_bytes, added_files, removed_files, changed_files })
    }).await.map_err(|error| error.to_string())?
}

#[tauri::command]
fn rename_snapshot(app: AppHandle, id: String, name: String) -> Result<SnapshotInfo, String> {
    let name = name.trim();
    if name.is_empty() { return Err("Snapshot name cannot be empty".into()); }
    let path = metadata_path(&app, &id)?;
    let mut info: SnapshotInfo = serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?).map_err(|error| error.to_string())?;
    info.root_path = clean_windows_path(&info.root_path);
    info.name = name.to_string();
    write_info(&path, &info)?;
    append_log(&app, format!("snapshot_renamed id={} name={}", info.id, info.name));
    Ok(info)
}

#[tauri::command]
fn move_snapshot(app: AppHandle, id: String, category: String) -> Result<SnapshotInfo, String> {
    let category = category.trim();
    if category.is_empty() { return Err("Category cannot be empty".into()); }
    let path = metadata_path(&app, &id)?;
    let mut info: SnapshotInfo = serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?).map_err(|error| error.to_string())?;
    info.root_path = clean_windows_path(&info.root_path);
    info.category = category.to_string();
    write_info(&path, &info)?;
    append_log(&app, format!("snapshot_category_changed id={} category={}", info.id, info.category));
    Ok(info)
}

#[tauri::command]
fn delete_snapshot(app: AppHandle, id: String) -> Result<(), String> {
    let data = snapshot_path(&app, &id)?;
    let metadata = metadata_path(&app, &id)?;
    if data.exists() { fs::remove_file(data).map_err(|error| error.to_string())?; }
    if metadata.exists() { fs::remove_file(metadata).map_err(|error| error.to_string())?; }
    append_log(&app, format!("snapshot_deleted id={id}"));
    Ok(())
}

#[tauri::command]
fn open_in_explorer(path: String, is_directory: Option<bool>) -> Result<(), String> {
    let target = PathBuf::from(clean_windows_path(path.trim().trim_matches('"')));
    let target_is_directory = is_directory.unwrap_or_else(|| target.is_dir());
    let mut directory = if target_is_directory {
        target.clone()
    } else {
        target.parent().map(Path::to_path_buf).unwrap_or(target.clone())
    };
    loop {
        if directory.is_dir() {
            let result = {
                #[cfg(windows)]
                {
                    let mut command = Command::new("explorer.exe");
                    if !target_is_directory {
                        command.raw_arg(format!("/select,\"{}\"", clean_path(&target)));
                    } else {
                        command.raw_arg(format!("/root,\"{}\"", clean_path(&target)));
                    }
                    command.spawn()
                }
                #[cfg(not(windows))]
                {
                    Command::new("explorer.exe").arg(clean_path(&directory)).spawn()
                }
            };
            return result.map(|_| ()).map_err(|error| error.to_string());
        }
        let Some(parent) = directory.parent().map(Path::to_path_buf) else {
            return Err("The selected path no longer exists".into());
        };
        if parent == directory {
            return Err("The selected path no longer exists".into());
        }
        directory = parent;
    }
}

#[tauri::command]
async fn select_directory(app: AppHandle) -> Option<String> {
    let (sender, receiver) = futures_channel::oneshot::channel();
    app.dialog()
        .file()
        .pick_folder(move |path| { let _ = sender.send(path); });
    receiver
        .await
        .ok()
        .flatten()
        .and_then(|path| path.into_path().ok())
        .map(|path| clean_path(&path))
}

#[tauri::command]
fn minimize_window(window: tauri::Window) -> Result<(), String> {
    window.minimize().map_err(|error| error.to_string())
}

#[tauri::command]
fn toggle_maximize_window(window: tauri::Window) -> Result<bool, String> {
    let maximized = window.is_maximized().map_err(|error| error.to_string())?;
    if maximized {
        window.unmaximize().map_err(|error| error.to_string())
    } else {
        window.maximize().map_err(|error| error.to_string())
    }?;
    Ok(!maximized)
}

#[tauri::command]
fn close_window(window: tauri::Window) -> Result<(), String> {
    window.close().map_err(|error| error.to_string())
}

#[tauri::command]
fn start_window_drag(window: tauri::Window) -> Result<(), String> {
    window.start_dragging().map_err(|error| error.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![get_snapshot_directory, get_app_version, set_snapshot_directory, reset_snapshot_directory, list_snapshot_folders, create_snapshot_folder, delete_snapshot_folder, get_app_log, clear_app_log, select_directory, list_snapshots, create_snapshot, get_snapshot_contents, compare_snapshots, rename_snapshot, move_snapshot, delete_snapshot, open_in_explorer, minimize_window, toggle_maximize_window, close_window, start_window_drag])
        .run(tauri::generate_context!())
        .expect("error while running TraceDisk");
}

#[cfg(test)]
mod tests {
    use super::{diff_snapshot_records, FileRecord};

    #[test]
    fn diff_marks_only_paths_missing_from_one_snapshot() {
        let old = vec![
            FileRecord { relative_path: "same.docx".into(), size_bytes: 100, modified_unix_ms: None },
            FileRecord { relative_path: "gone.bin".into(), size_bytes: 0, modified_unix_ms: None },
        ];
        let new = vec![
            FileRecord { relative_path: "same.docx".into(), size_bytes: 1, modified_unix_ms: None },
            FileRecord { relative_path: "added.jpg".into(), size_bytes: 0, modified_unix_ms: None },
        ];

        let (added, removed, changed, added_bytes, removed_bytes) = diff_snapshot_records(&old, &new);

        assert_eq!(added.len(), 1);
        assert_eq!(added[0].path, "added.jpg");
        assert_eq!(added[0].status, "added");
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].path, "gone.bin");
        assert_eq!(removed[0].status, "removed");
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].path, "same.docx");
        assert_eq!(changed[0].status, "changed");
        assert_eq!(changed[0].previous_size_bytes, Some(100));
        assert_eq!(changed[0].current_size_bytes, Some(1));
        assert_eq!(added_bytes, 0);
        assert_eq!(removed_bytes, 0);
    }

}
