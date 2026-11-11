#!/usr/bin/env bash
set -euo pipefail
if [ "$(id -u)" = 0 ]; then
  # Optional local WSL GPU validation. The complete read-only WSL driver tree
  # must be mounted; mounting only Windows' lxss/lib omits the display driver.
  if [ "${RECEOS_WSL_GPU:-0}" = 1 ]; then
    test -c /dev/dxg
    test -f /usr/lib/wsl/lib/libd3d12.so
    test -f /usr/lib/wsl/lib/libnvwgf2umx.so
    printf '/usr/lib/wsl/lib\n' > /etc/ld.so.conf.d/receos-wsl-graphics.conf
    ldconfig
  fi
  package_dir=${RECEOS_PACKAGE_DIR:-/workspace/src-tauri/target/linux-validation/account-free/release-ubuntu22}
  packages=("$package_dir"/*.deb)
  dpkg -i "${packages[@]}"
  chown validation:validation /home/validation
  exec runuser -u validation -- env HOME=/home/validation "$0"
fi

export DISPLAY=:99 XDG_RUNTIME_DIR=/tmp/receos-playtest-runtime
export LIBGL_ALWAYS_SOFTWARE=1 XDG_SESSION_TYPE=x11
mkdir -p "$XDG_RUNTIME_DIR" "$HOME/.vnc" "$HOME/playtest-logs"
chmod 700 "$XDG_RUNTIME_DIR" "$HOME/.vnc"
if [ ! -f "$HOME/.vnc/session-password.txt" ]; then
  python3 -c 'import os,secrets; p=os.path.expanduser("~/.vnc/session-password.txt"); fd=os.open(p,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600); os.write(fd,secrets.token_urlsafe(12)[:8].encode()); os.close(fd)'
  x11vnc -storepasswd "$(cat "$HOME/.vnc/session-password.txt")" "$HOME/.vnc/passwd" >/dev/null 2>&1
fi
chmod 600 "$HOME/.vnc/passwd"
# Reuse only the pinned archive extraction made by the validation suite. The
# optional host seed is mounted read-only; prefixes and game writes stay here.
client="$HOME/.local/share/com.receos.launcher/client-receos"
if [ -f /opt/receos-seed/.radium-files ] && [ ! -e "$client" ]; then
  mkdir -p "$client.seed"
  cp -R /opt/receos-seed/. "$client.seed/"
  mv --no-clobber -T "$client.seed" "$client"
fi
# Container restarts preserve /tmp but kill the old X server. Remove only its
# stale display-99 files; never disturb a live server or another user's files.
python3 - <<'PY'
import os
from pathlib import Path
import stat
for proc in Path('/proc').iterdir():
    if not proc.name.isdecimal():
        continue
    try:
        if (proc / 'comm').read_text().strip() not in ('Xvfb', 'Xorg', 'Xwayland'):
            continue
        if b':99' in (proc / 'cmdline').read_bytes().split(b'\0'):
            raise RuntimeError('Display :99 already has a live X server')
    except (FileNotFoundError, ProcessLookupError):
        pass
for name, kind in (('/tmp/.X99-lock', stat.S_ISREG), ('/tmp/.X11-unix/X99', stat.S_ISSOCK)):
    path = Path(name)
    try:
        info = path.lstat()
    except FileNotFoundError:
        continue
    if info.st_uid != os.getuid() or not kind(info.st_mode):
        raise RuntimeError('Unexpected owner/type for stale display file: ' + name)
    path.unlink()
PY
env -u GALLIUM_DRIVER -u MESA_D3D12_DEFAULT_ADAPTER_NAME LIBGL_ALWAYS_SOFTWARE=1 \
  Xvfb "$DISPLAY" -screen 0 1440x900x24 -ac -nolisten tcp > "$HOME/playtest-logs/xvfb.log" 2>&1 &
for attempt in {1..50}; do
  if xdpyinfo -display "$DISPLAY" >/dev/null 2>&1; then break; fi
  sleep 0.2
done
xdpyinfo -display "$DISPLAY" >/dev/null

if [ "${RECEOS_WSL_GPU:-0}" = 1 ]; then
  export LIBGL_ALWAYS_SOFTWARE=0 GALLIUM_DRIVER=d3d12
  export MESA_D3D12_DEFAULT_ADAPTER_NAME=${RECEOS_WSL_ADAPTER:-NVIDIA}
  export PRESSURE_VESSEL_VARIABLE_DIR="$HOME/.cache/receos-gpu-runtime-probe"
  export PRESSURE_VESSEL_BWRAP=/usr/local/bin/receos-wsl-gpu-bwrap
  export RECEOS_REAL_BWRAP="$HOME/.local/share/Steam/steamapps/common/SteamLinuxRuntime_4/pressure-vessel/libexec/steam-runtime-tools-0/srt-bwrap"
  test -x "$RECEOS_REAL_BWRAP"
fi

exec dbus-run-session -- bash -c '
  umask 077
  printf "export DBUS_SESSION_BUS_ADDRESS=%q\nexport DISPLAY=%q\nexport XDG_RUNTIME_DIR=%q\n" "$DBUS_SESSION_BUS_ADDRESS" "$DISPLAY" "$XDG_RUNTIME_DIR" > "$HOME/.desktop-session.env"
  for name in LIBGL_ALWAYS_SOFTWARE GALLIUM_DRIVER MESA_D3D12_DEFAULT_ADAPTER_NAME PRESSURE_VESSEL_VARIABLE_DIR PRESSURE_VESSEL_BWRAP RECEOS_REAL_BWRAP; do
    if [[ -v $name ]]; then printf "export %s=%q\n" "$name" "${!name}" >> "$HOME/.desktop-session.env"; fi
  done
  xfce4-session > "$HOME/playtest-logs/desktop.log" 2>&1 &
  x11vnc -display "$DISPLAY" -localhost -forever -shared -rfbauth "$HOME/.vnc/passwd" -rfbport 5900 > "$HOME/playtest-logs/vnc.log" 2>&1 &
  websockify --web=/usr/share/novnc 0.0.0.0:6080 127.0.0.1:5900 > "$HOME/playtest-logs/websockify.log" 2>&1 &
  # Steam itself uses additional runtimes; keep the WSL adapter scoped to games.
  env -u PRESSURE_VESSEL_BWRAP -u PRESSURE_VESSEL_VARIABLE_DIR \
    -u RECEOS_REAL_BWRAP -u GALLIUM_DRIVER LIBGL_ALWAYS_SOFTWARE=1 \
    steam > "$HOME/playtest-logs/steam-bootstrap.log" 2>&1 &
  wait
'
