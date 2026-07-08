import { readFileSync, writeFileSync } from 'fs';
import { resolve, dirname } from 'path';
import { fileURLToPath } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(__dirname, '..');

function read(path) {
  return readFileSync(resolve(ROOT, path), 'utf-8');
}

function write(path, content) {
  writeFileSync(resolve(ROOT, path), content);
}

const packageJson = JSON.parse(read('package.json'));
const version = packageJson.version;

if (!version) {
  console.error('Error: no version found in package.json');
  process.exit(1);
}

console.log(`Syncing version ${version} to all files...`);

// 1. src-tauri/tauri.conf.json (Tauri v2 — top-level "version")
let tauriV2 = JSON.parse(read('src-tauri/tauri.conf.json'));
tauriV2.version = version;
write('src-tauri/tauri.conf.json', JSON.stringify(tauriV2, null, 2) + '\n');
console.log('  ✓ src-tauri/tauri.conf.json');

// 2. src-tauri-32bit/tauri.conf.json (Tauri v1 — version under "package")
let tauriV1 = JSON.parse(read('src-tauri-32bit/tauri.conf.json'));
tauriV1.package.version = version;
write('src-tauri-32bit/tauri.conf.json', JSON.stringify(tauriV1, null, 2) + '\n');
console.log('  ✓ src-tauri-32bit/tauri.conf.json');

// 3. src-tauri/Cargo.toml
let cargo = read('src-tauri/Cargo.toml');
cargo = cargo.replace(/^version = ".*?"/m, `version = "${version}"`);
write('src-tauri/Cargo.toml', cargo);
console.log('  ✓ src-tauri/Cargo.toml');

// 4. src-tauri-32bit/Cargo.toml
cargo = read('src-tauri-32bit/Cargo.toml');
cargo = cargo.replace(/^version = ".*?"/m, `version = "${version}"`);
write('src-tauri-32bit/Cargo.toml', cargo);
console.log('  ✓ src-tauri-32bit/Cargo.toml');

// 5. audio-core/Cargo.toml
cargo = read('audio-core/Cargo.toml');
cargo = cargo.replace(/^version = ".*?"/m, `version = "${version}"`);
write('audio-core/Cargo.toml', cargo);
console.log('  ✓ audio-core/Cargo.toml');

console.log(`\nAll files synced to version ${version}.`);
console.log('Run `npm install` to update package-lock.json.');
console.log('Run `cargo build` in each crate to update Cargo.lock files.');