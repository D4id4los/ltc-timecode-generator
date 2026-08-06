import { readFileSync, writeFileSync } from 'fs';
import { execSync } from 'child_process';
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

function run(cmd, cwd) {
  console.log(`  → ${cmd} (in ${resolve(ROOT, cwd)})`);
  execSync(cmd, { cwd: resolve(ROOT, cwd), stdio: 'inherit' });
}

// --- Sync version fields ---

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

// 6. ltc-gui/Cargo.toml
cargo = read('ltc-gui/Cargo.toml');
cargo = cargo.replace(/^version = ".*?"/m, `version = "${version}"`);
write('ltc-gui/Cargo.toml', cargo);
console.log('  ✓ ltc-gui/Cargo.toml');

// --- Update lock files ---

console.log('\nUpdating lock files...');

run('npm install', '.');
console.log('  ✓ package-lock.json');

run('cargo generate-lockfile', '.');
console.log('  ✓ Cargo.lock (workspace: audio-core + ltc-gui)');

run('cargo generate-lockfile --manifest-path src-tauri/Cargo.toml', '.');
console.log('  ✓ src-tauri/Cargo.lock');

run('cargo generate-lockfile --manifest-path src-tauri-32bit/Cargo.toml', '.');
console.log('  ✓ src-tauri-32bit/Cargo.lock');

// --- Stage all changed files for the npm version commit ---

console.log('\nStaging all changed files for git commit...');
run('git add package.json package-lock.json Cargo.lock', '.');
run('git add src-tauri/tauri.conf.json src-tauri/Cargo.toml src-tauri/Cargo.lock', '.');
run('git add src-tauri-32bit/tauri.conf.json src-tauri-32bit/Cargo.toml src-tauri-32bit/Cargo.lock', '.');
run('git add audio-core/Cargo.toml', '.');
run('git add ltc-gui/Cargo.toml', 'ltc-gui');
console.log('  ✓ All files staged');

console.log(`\nAll files synced to version ${version}. Lock files are up to date.`);