import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const resources = path.join(root, 'src-tauri', 'resources');
fs.mkdirSync(resources, { recursive: true });
const helper = path.join(resources, 'receos-proton-helper.exe');
if (process.argv.includes('--build-helper')) {
  if (process.platform !== 'win32') throw new Error('Build the Proton helper on Windows, then copy it to the Linux builder.');
  const manifest = path.join(root, 'src-tauri', 'compat-helper', 'Cargo.toml');
  execFileSync('cargo', ['build', '--locked', '--release', '--target', 'x86_64-pc-windows-msvc', '--manifest-path', manifest], {
    stdio: 'inherit', env: { ...process.env, RUSTFLAGS: [process.env.RUSTFLAGS, '-C target-feature=+crt-static'].filter(Boolean).join(' ') },
  });
  fs.copyFileSync(path.join(root, 'src-tauri/compat-helper/target/x86_64-pc-windows-msvc/release/receos-proton-helper.exe'), helper);
  console.log(`Prepared ${helper}`);
} else {
  if (process.platform !== 'linux') throw new Error('Native Linux packages must be built on Linux. Use the Linux CI workflow from Windows.');
  if (process.env.RECEOS_PROTON_HELPER) fs.copyFileSync(process.env.RECEOS_PROTON_HELPER, helper);
  if (!fs.existsSync(helper)) throw new Error('Missing Windows Proton helper. Download the helper artifact from CI, or prepare it on Windows with npm run prepare:proton-helper.');
  const metadata = JSON.parse(execFileSync('cargo', ['metadata', '--locked', '--format-version', '1', '--manifest-path', path.join(root, 'src-tauri/Cargo.toml')], { encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 }));
  const sdk = metadata.packages.find(p => p.name === 'steamworks-sys');
  if (!sdk) throw new Error('steamworks-sys was not found in Cargo metadata.');
  const sdkRoot = process.env.STEAM_SDK_LOCATION || path.join(path.dirname(sdk.manifest_path), 'lib', 'steam');
  fs.copyFileSync(path.join(sdkRoot, 'redistributable_bin/linux64/libsteam_api.so'), path.join(resources, 'libsteam_api.so'));
  console.log('Prepared Linux Steam SDK library and Proton helper.');
}
