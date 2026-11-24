// Signs this version's installers for the launcher's in-app updater, after
// `tauri build` (`npm run build` runs it). Each `...-setup.exe` gets a
// `...-setup.exe.sig` beside it; a Linux AppImage named on the command line
// gets its `.AppImage.sig`. Upload BOTH to the GitHub release: a release
// without the .sig is offered to users as its web page instead of being
// installed from the launcher (see verify_update_signature in updater.rs).
//
// The key is the one updater.rs checks against (UPDATE_PUBLIC_KEY), kept
// outside the repo at %USERPROFILE%\.tauri\radium-launcher-updates.key, or
// wherever TAURI_SIGNING_PRIVATE_KEY_PATH says. Its password, if it has one,
// comes from TAURI_SIGNING_PRIVATE_KEY_PASSWORD. Losing the key means no
// launcher can update itself again until a new version with a new key is
// installed by hand, so keep a backup of it somewhere safe.

import { createRequire } from 'node:module';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

function fail(message) {
  console.error(`\nsign-update: ${message}\n`);
  process.exit(1);
}

const keyPath = process.env.TAURI_SIGNING_PRIVATE_KEY_PATH
  || path.join(os.homedir(), '.tauri', 'radium-launcher-updates.key');
if (!fs.existsSync(keyPath)) {
  fail(`No signing key at ${keyPath}. The installer was built but NOT signed, so the launcher won't install it as an update.`);
}

// The key has to be the one the launcher trusts, or every update signed with
// it would be refused. Checked here rather than found out after release.
const updater = fs.readFileSync(path.join(root, 'src-tauri', 'src', 'updater.rs'), 'utf8');
const trusted = updater.match(/const UPDATE_PUBLIC_KEY: &str = "([^"]+)";/)?.[1];
const ours = fs.existsSync(`${keyPath}.pub`) ? fs.readFileSync(`${keyPath}.pub`, 'utf8').trim() : '';
if (!trusted) fail('Could not find UPDATE_PUBLIC_KEY in src-tauri/src/updater.rs.');
if (ours !== trusted) {
  fail(`${keyPath}.pub is not the key in updater.rs (UPDATE_PUBLIC_KEY). Signing with it would make the update fail to install.`);
}

const version = JSON.parse(fs.readFileSync(path.join(root, 'src-tauri', 'tauri.conf.json'), 'utf8')).version;
const target = path.join(root, 'src-tauri', 'target');
const bundleDirs = [path.join(target, 'release', 'bundle', 'nsis')];
for (const entry of fs.existsSync(target) ? fs.readdirSync(target, { withFileTypes: true }) : []) {
  if (entry.isDirectory()) bundleDirs.push(path.join(target, entry.name, 'release', 'bundle', 'nsis'));
}
// Files named on the command line are signed instead: the Linux AppImage,
// which CI builds, is downloaded and signed here, since the key never leaves
// this PC: `npm run sign:update -- "path/to/Rec EOS Launcher_<v>_amd64.AppImage"`.
const named = process.argv.slice(2).map((file) => path.resolve(file));
for (const file of named) {
  if (!fs.existsSync(file)) fail(`${file} doesn't exist.`);
  if (!/(-setup\.exe|\.AppImage)$/.test(file)) fail(`${file} isn't a -setup.exe or an .AppImage.`);
  if (!path.basename(file).includes(`_${version}_`)) fail(`${file} isn't version ${version} (tauri.conf.json).`);
}
const installers = named.length ? named : bundleDirs
  .filter((dir) => fs.existsSync(dir))
  .flatMap((dir) => fs.readdirSync(dir).map((name) => path.join(dir, name)))
  .filter((file) => file.endsWith('-setup.exe') && path.basename(file).includes(`_${version}_`));
if (installers.length === 0) fail(`No ${version} installer found under src-tauri/target. Build first.`);

const { run } = require('@tauri-apps/cli');
const password = process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD ?? '';
for (const installer of installers) {
  await run(['signer', 'sign', '--private-key-path', keyPath, '--password', password, installer], 'tauri')
    .catch((e) => fail(`Signing ${installer} failed: ${e?.message || e}`));
  if (!fs.existsSync(`${installer}.sig`)) fail(`No signature was written for ${installer}.`);
  console.log(`Signed: ${installer}.sig`);
}
console.log('\nUpload each -setup.exe and .AppImage together with its .sig to the GitHub release.');
