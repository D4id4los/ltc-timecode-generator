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

const TAURI_CONFS = [
  {
    path: 'src-tauri/tauri.conf.json',
    apply: (json) => { json.version = version; },
  },
  {
    path: 'src-tauri-32bit/tauri.conf.json',
    apply: (json) => { json.package.version = version; },
  },
];

for (const { path, apply } of TAURI_CONFS) {
  const json = JSON.parse(read(path));
  apply(json);
  write(path, JSON.stringify(json, null, 2) + '\n');
  console.log(`  ✓ ${path}`);
}

const CARGO_MANIFESTS = [
  'audio-core/Cargo.toml',
  'gui-engine/Cargo.toml',
  'ltc-gui/Cargo.toml',
  'ltc-slint/Cargo.toml',
  'src-tauri/Cargo.toml',
  'src-tauri-32bit/Cargo.toml',
];

const escapedVersion = version.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
const packageVersionLine = new RegExp(`^version = "${escapedVersion}"`, 'm');

for (const manifest of CARGO_MANIFESTS) {
  let cargo = read(manifest);
  cargo = cargo.replace(/^version = ".*?"/m, `version = "${version}"`);
  if (!packageVersionLine.test(cargo)) {
    console.error(`Error: failed to set [package] version in ${manifest}`);
    process.exit(1);
  }
  write(manifest, cargo);
  console.log(`  ✓ ${manifest}`);
}

// --- Update lock files ---

console.log('\nUpdating lock files...');

run('npm install', '.');
console.log('  ✓ package-lock.json');

run('cargo update --workspace', '.');
console.log('  ✓ Cargo.lock (workspace: audio-core + gui-engine + ltc-gui + ltc-slint)');

run('cargo update --workspace --manifest-path src-tauri/Cargo.toml', '.');
console.log('  ✓ src-tauri/Cargo.lock');

run('cargo update --workspace --manifest-path src-tauri-32bit/Cargo.toml', '.');
console.log('  ✓ src-tauri-32bit/Cargo.lock');

// --- Stage all changed files for the npm version commit ---

console.log('\nStaging all changed files for git commit...');
run(
  'git add package.json package-lock.json Cargo.lock ' +
    'src-tauri/tauri.conf.json src-tauri/Cargo.toml src-tauri/Cargo.lock ' +
    'src-tauri-32bit/tauri.conf.json src-tauri-32bit/Cargo.toml src-tauri-32bit/Cargo.lock ' +
    'audio-core/Cargo.toml gui-engine/Cargo.toml ltc-gui/Cargo.toml ltc-slint/Cargo.toml',
  '.',
);
console.log('  ✓ All files staged');

console.log(`\nAll files synced to version ${version}. Lock files are up to date.`);
