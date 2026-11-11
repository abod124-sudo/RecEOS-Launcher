#!/usr/bin/python3
"""Local WSL test adapter: add read-only graphics files to Steam's runtime.

Keep the real bubblewrap executable and all its namespace/isolation options.
Only a private runtime copy under this test user's cache may be extended.
This adapter is not packaged with the launcher or used by native Linux.
"""
import os
import sys

arguments = list(sys.argv[1:])
expanded = []
while arguments:
    value = arguments.pop(0)
    if value == '--args':
        descriptor = int(arguments.pop(0))
        data = bytearray()
        while chunk := os.read(descriptor, 65536):
            data.extend(chunk)
        items = bytes(data).split(b'\0')
        if items[-1] == b'':
            items.pop()
        expanded.extend(os.fsdecode(item) for item in items)
    else:
        expanded.append(value)

indices = [i for i, value in enumerate(expanded)
           if os.path.basename(value) in ('pv-adverb', 'srt-adverb')]
if indices and '/usr' in expanded:
    mounts = [expanded[i + 1] for i, value in enumerate(expanded[:-2])
              if value in ('--ro-bind', '--bind', '--ro-bind-try', '--bind-try')
              and expanded[i + 2] == '/usr']
    if not mounts:
        raise RuntimeError('Runtime /usr mount missing')
    source = os.path.realpath(mounts[-1])
    private_cache = os.path.realpath(os.path.expanduser('~/.cache/receos-gpu-runtime-probe')) + '/'
    if not source.startswith(private_cache):
        raise RuntimeError('WSL adapter requires its private runtime cache')
    os.makedirs(os.path.join(source, 'lib/wsl'), exist_ok=True)
    for name in ('libd3d12.so', 'libd3d12core.so'):
        link = os.path.join(source, 'lib/x86_64-linux-gnu', name)
        if not os.path.lexists(link):
            os.symlink('/usr/lib/wsl/lib/' + name, link)
    expanded[indices[0]:indices[0]] = ['--ro-bind', '/usr/lib/wsl', '/usr/lib/wsl']
os.execv(os.environ['RECEOS_REAL_BWRAP'], [os.environ['RECEOS_REAL_BWRAP'], *expanded])
