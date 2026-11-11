#!/usr/bin/env bash
set -euo pipefail
display=${1:?Usage: desktop.sh x11|wayland APPLICATION OUTPUT}
application=${2:?Application executable required}
output=${3:?Output directory required}
# Older distro Weston headless backends have no wl_seat. GTK3 automation can
# then hang taking a snapshot after scrolling. Nest Weston on isolated Xvfb
# instead: its X11 backend supplies virtual pointer/keyboard devices while the
# application still uses native Wayland (or Weston's XWayland for AppImage).
if [ "$display" = wayland ] && [ "${RECEOS_WESTON_X11_READY:-}" != 1 ]; then
  exec xvfb-run -a env RECEOS_WESTON_X11_READY=1 bash "$0" "$@"
fi
export WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1
export WEBKIT_DISABLE_COMPOSITING_MODE=1
mkdir -p "$output"
export XDG_RUNTIME_DIR
XDG_RUNTIME_DIR=$(mktemp -d -t receos-desktop-runtime-XXXXXXXX)
cleanup() {
  if [ -n "${compositor:-}" ]; then kill "$compositor" 2>/dev/null || true; fi
  if [ -n "${audio:-}" ]; then kill "$audio" 2>/dev/null || true; fi
}
trap cleanup EXIT
# A real audio service with a silent output exercises WebAudio without
# requiring a sound device or playing sounds on the development machine.
pulseaudio --daemonize=no --exit-idle-time=-1 --file=/dev/null \
  --load="module-native-protocol-unix socket=$XDG_RUNTIME_DIR/pulse-native auth-anonymous=1" \
  --load="module-null-sink sink_name=validation" > "$output/pulseaudio.log" 2>&1 &
audio=$!
for attempt in {1..50}; do
  if [ -S "$XDG_RUNTIME_DIR/pulse-native" ]; then break; fi
  sleep 0.2
done
test -S "$XDG_RUNTIME_DIR/pulse-native"
export PULSE_SERVER="unix:$XDG_RUNTIME_DIR/pulse-native"
if [ "$display" = wayland ]; then
  weston --backend=x11-backend.so --use-pixman --socket=wayland-validation --idle-time=0 --xwayland \
    --width=1280 --height=800 --log="$output/weston.log" &
  compositor=$!
  for attempt in {1..50}; do
    if [ -S "$XDG_RUNTIME_DIR/wayland-validation" ]; then break; fi
    sleep 0.2
  done
  test -S "$XDG_RUNTIME_DIR/wayland-validation"
  export WAYLAND_DISPLAY=wayland-validation GDK_BACKEND=wayland
  # Tauri's AppImage GTK hook selects X11. Give it this Wayland session's
  # XWayland display, while native .deb windows use WAYLAND_DISPLAY directly.
  for attempt in {1..50}; do
    if grep -q 'xserver listening on display' "$output/weston.log"; then break; fi
    sleep 0.2
  done
  export RECEOS_APPLICATION_DISPLAY
  RECEOS_APPLICATION_DISPLAY=$(sed -n 's/.*xserver listening on display \(:[0-9]*\).*/\1/p' "$output/weston.log" | head -1)
  test -n "$RECEOS_APPLICATION_DISPLAY"
else
  export GDK_BACKEND=x11
fi
# Dunst uses the isolated X server in both cases. The application itself is
# forced to the selected backend; notification dispatch uses the real D-Bus API.
xvfb-run -a dbus-run-session -- python3 tests/linux/desktop.py "$application" --output "$output"
