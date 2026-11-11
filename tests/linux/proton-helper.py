#!/usr/bin/env python3
"""Exercise the real Windows patch helper inside an installed Valve Proton.

Uses private Windows fixtures built by compat-helper's Windows integration
tests. No game authentication or game compatibility result is simulated.
The runtime-status JSON must come from get_linux_runtime_status. Everything
created by this test stays in a fresh output directory, including its prefix.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import shutil
import signal
import subprocess
import tempfile
import time


def owned_games(prefix, game):
    found = []
    expected = prefix.resolve()
    image = str(game.resolve())
    for proc in Path('/proc').iterdir():
        if not proc.name.isdecimal():
            continue
        try:
            if proc.stat().st_uid != os.getuid():
                continue
            env = (proc / 'environ').read_bytes().split(b'\0')
            value = next((item.split(b'=', 1)[1] for item in env if item.startswith(b'WINEPREFIX=')), None)
            if value is None or Path(os.fsdecode(value)).resolve() != expected:
                continue
            if not any(line.endswith(' ' + image) for line in (proc / 'maps').read_text().splitlines()):
                continue
            found.append(int(proc.name))
        except (OSError, UnicodeError):
            continue
    return found


def stop_owned(prefix, game):
    for pid in owned_games(prefix, game):
        try:
            fd = os.pidfd_open(pid)
        except ProcessLookupError:
            continue
        try:
            # Recheck ownership after obtaining the handle, guarding PID reuse.
            if pid in owned_games(prefix, game):
                signal.pidfd_send_signal(fd, signal.SIGKILL)
                poll = select.poll()
                poll.register(fd, select.POLLIN)
                assert poll.poll(10000), 'Owned fixture did not exit'
        finally:
            os.close(fd)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--runtime-status', type=Path, required=True)
    parser.add_argument('--fixtures', type=Path, required=True)
    parser.add_argument('--helper', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    status = json.loads(args.runtime_status.read_text())
    assert status['ready'], status.get('error')
    args.output.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix='proton-helper-', dir=args.output))
    compat = root / 'compat data'
    compat.mkdir()
    helper = compat / 'receos-proton-helper.exe'
    shutil.copyfile(args.helper, helper)
    environment = dict(os.environ)
    for key in ('LD_LIBRARY_PATH', 'LD_PRELOAD', 'GTK_PATH', 'GTK_EXE_PREFIX', 'GTK_DATA_PREFIX', 'GIO_EXTRA_MODULES'):
        environment.pop(key, None)
    environment.update({
        'STEAM_COMPAT_DATA_PATH': str(compat),
        'STEAM_COMPAT_CLIENT_INSTALL_PATH': status['steamDir'],
        'STEAM_COMPAT_APP_ID': '471710', 'SteamAppId': '471710', 'SteamGameId': '471710',
        'STEAM_COMPAT_LIBRARY_PATHS': status['steamDir'],
        'STEAM_COMPAT_TOOL_PATHS': os.pathsep.join((status['protonDir'], str(Path(status['runtime']).parent))),
        'PRESSURE_VESSEL_FILESYSTEMS_RW': str(root),
    })

    def command(verb, *arguments):
        return [status['runtime'], '--verb=run', '--', str(Path(status['protonDir']) / 'proton'), verb, *map(str, arguments)]

    def windows_path(path):
        with open(root / 'path-conversion.log', 'ab') as log:
            result = subprocess.run(command('getcompatpath', path), env=environment,
                                    stdout=subprocess.PIPE, stderr=log, timeout=180, check=True)
        value = result.stdout.decode().strip()
        assert value and '\n' not in value and '\r' not in value, value
        return value

    results = []
    for name, patch, arguments, expected in (
        ('valid patch', 'module.dll', [], True),
        ('invalid patch', None, [], False),
        ('unrelated same basename', None, ['preload-other'], False),
        ('exit during load', 'slow.dll', ['exit-during-load'], False),
    ):
        case = root / name
        (case / 'verified').mkdir(parents=True)
        (case / 'other').mkdir()
        game = case / 'RecRoom.exe'
        shutil.copyfile(args.fixtures / 'game.exe', game)
        shutil.copyfile(args.fixtures / 'module.dll', case / 'GameAssembly.dll')
        shutil.copyfile(args.fixtures / 'module.dll', case / 'other/patch.dll')
        target = case / 'verified/patch.dll'
        target.write_bytes((args.fixtures / patch).read_bytes() if patch else b'This is not a PE library')
        request = {'version': 1, 'exe': windows_path(game), 'patch': windows_path(target),
                   'sha256': hashlib.sha256(target.read_bytes()).hexdigest(), 'args': arguments}
        input_path = compat / f'{name}-request.json'
        output_path = compat / f'{name}-response.json'
        input_path.write_text(json.dumps(request))
        input_path.chmod(0o600)
        protocol = ['--request-file', windows_path(input_path), '--response-file', windows_path(output_path)]
        environment['STEAM_COMPAT_INSTALL_PATH'] = str(case)
        with open(case / 'runtime.log', 'wb') as log:
            process = subprocess.Popen(command('run', helper, *protocol), cwd=case, env=environment,
                                       stdin=subprocess.DEVNULL, stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 180
                while not output_path.exists() and time.monotonic() < deadline:
                    if process.poll() is not None:
                        break
                    time.sleep(0.1)
                assert output_path.exists(), f'{name}: helper did not report a result; inspect {case / "runtime.log"}'
                reply = json.loads(output_path.read_bytes())
                assert reply.get('version') == 1 and reply.get('success') is expected, reply
                alive = bool(owned_games(compat / 'pfx', game))
                assert alive is expected, f'{name}: unexpected owned game state {alive}'
                result = {'case': name, 'passed': True, 'patchLoaded': expected,
                          'gameRunningAfterReply': alive, 'reply': reply}
                results.append(result)
                print(json.dumps(result), flush=True)
            finally:
                stop_owned(compat / 'pfx', helper)
                stop_owned(compat / 'pfx', game)
                try:
                    process.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    process.terminate()
                    process.wait(timeout=10)
                (root / 'results.json').write_text(json.dumps(results, indent=2))
    # Exercise Radium's legacy cmd launch with a real detached PE child and
    # spaces in its batch path. This is a fixture, not a downloaded game.
    case = root / 'legacy batch with spaces'
    case.mkdir()
    game = case / 'RecRoom.exe'
    shutil.copyfile(args.fixtures / 'game.exe', game)
    shutil.copyfile(args.fixtures / 'module.dll', case / 'GameAssembly.dll')
    batch = case / 'launch game.bat'
    batch.write_bytes(b'@echo off\r\necho BAT_STARTED>"%~dp0batch-started.txt"\r\nstart "" "%~dp0RecRoom.exe"\r\nexit /b 0\r\n')
    path = windows_path(batch)
    environment.update({'STEAM_COMPAT_APP_ID': '92', 'SteamAppId': '92', 'SteamGameId': '92',
                        'STEAM_COMPAT_INSTALL_PATH': str(case)})
    with open(case / 'runtime.log', 'wb') as log:
        process = subprocess.Popen(command('run', 'cmd.exe', '/d', '/s', '/c', 'call', path),
                                   cwd=case, env=environment, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 90
            while not owned_games(compat / 'pfx', game) and time.monotonic() < deadline:
                time.sleep(0.1)
            assert owned_games(compat / 'pfx', game), f'Batch launch failed; inspect {case / "runtime.log"}'
            result = {'case': 'legacy cmd batch with spaces', 'passed': True,
                      'actualPeProcessRunning': True, 'wrapperExitedBeforeStop': process.poll() is not None}
            results.append(result)
            print(json.dumps(result), flush=True)
        finally:
            stop_owned(compat / 'pfx', game)
            process.wait(timeout=30)
            (root / 'results.json').write_text(json.dumps(results, indent=2))
    print(json.dumps({'passed': len(results), 'results': str(root / 'results.json'),
                      'realGameplayValidated': False}), flush=True)


if __name__ == '__main__':
    main()
