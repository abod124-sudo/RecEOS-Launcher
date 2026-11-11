import { spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const require = createRequire(import.meta.url);
const { run } = require('@tauri-apps/cli');
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const dev = args.includes('--dev');
const linux = args.includes('--linux');
if (linux && process.platform !== 'linux') throw new Error('Linux builds require a Linux host. On Windows, use the Linux CI workflow.');
const cliArgs = args.filter(a => !['--dev', '--linux', '--unsigned'].includes(a));
if (process.platform === 'linux') {
  const prep = spawnSync(process.execPath, [path.join(root, 'scripts/prepare-linux.mjs')], { cwd: root, stdio: 'inherit' });
  if (prep.status !== 0) process.exit(prep.status || 1);
}
await run([dev ? 'dev' : 'build', ...cliArgs], 'tauri');
if (!dev && process.platform === 'win32' && args.includes('--unsigned')) {
  const targetRoot = path.join(root, 'src-tauri', 'target');
  const targetIndex = cliArgs.indexOf('--target');
  const target = targetIndex < 0 ? '' : cliArgs[targetIndex + 1];
  const profile = cliArgs.includes('--debug') ? 'debug' : 'release';
  const bundleDir = path.resolve(targetRoot, target, profile, 'bundle', 'nsis');
  if (!bundleDir.startsWith(targetRoot + path.sep)) throw new Error('Unexpected bundle directory.');
  const version = JSON.parse(fs.readFileSync(path.join(root, 'src-tauri/tauri.conf.json'), 'utf8')).version;
  for (const name of fs.existsSync(bundleDir) ? fs.readdirSync(bundleDir) : []) {
    if (name.includes(`_${version}_`) && name.endsWith('-setup.exe')) {
      const signature = path.join(bundleDir, `${name}.sig`);
      if (fs.existsSync(signature)) fs.unlinkSync(signature);
    }
  }
}
if (!dev && process.platform === 'win32' && !args.includes('--unsigned')) {
  // The native CLI may change process.cwd() to src-tauri while building.
  const sign = spawnSync(process.execPath, [path.join(root, 'scripts/sign-update.mjs')], { cwd: root, stdio: 'inherit' });
  if (sign.error) throw sign.error;
  process.exit(sign.status ?? 1);
}
