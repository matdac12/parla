// Sets the app version everywhere the release workflow checks it.
// Usage: node scripts/set-version.mjs 0.2.0
import { readFileSync, writeFileSync } from "node:fs";

const version = process.argv[2];
if (!/^\d+\.\d+\.\d+$/.test(version ?? "")) {
  console.error("usage: node scripts/set-version.mjs <major.minor.patch>");
  process.exit(1);
}

// Each pattern captures the text before and after the version string.
function edit(path, pattern) {
  const text = readFileSync(path, "utf8");
  if (!pattern.test(text)) throw new Error(`${path}: version field not found`);
  writeFileSync(path, text.replace(pattern, `$1${version}$2`));
}

const jsonVersion = /("version":\s*")[^"]+(")/;
edit("package.json", jsonVersion);
edit("src-tauri/tauri.conf.json", jsonVersion);
// The root entry and its packages[""] mirror.
edit("package-lock.json", /^(\{\s*"name":\s*"parla",\s*"version":\s*")[^"]+(")/);
edit("package-lock.json", /("packages":\s*\{\s*"":\s*\{\s*"name":\s*"parla",\s*"version":\s*")[^"]+(")/);
edit("src-tauri/Cargo.toml", /^(\[package\]\r?\nname = "parla"\r?\nversion = ")[^"]+(")/);
edit("src-tauri/Cargo.lock", /(\[\[package\]\]\r?\nname = "parla"\r?\nversion = ")[^"]+(")/);

console.log(`Version set to ${version}. Commit, then tag v${version}.`);
