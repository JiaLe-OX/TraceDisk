import { copyFile, mkdir } from "node:fs/promises";

await mkdir(new URL("../dist/", import.meta.url), { recursive: true });
await copyFile(new URL("../index.html", import.meta.url), new URL("../dist/index.html", import.meta.url));
