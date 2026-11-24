#!/usr/bin/env bash
# Exercise the AppImage's layout, library loading, and frontend startup without
# Steam/accounts. Gameplay and desktop integration still need the manual matrix.
set -euo pipefail
image=$(realpath "${1:?Usage: smoke-linux.sh PACKAGE.AppImage}")
scratch=$(mktemp -d -t receos-smoke-XXXXXXXX)
mkdir "$scratch/image"
(cd "$scratch/image" && env -u APPIMAGE_EXTRACT_AND_RUN "$image" --appimage-extract > "$scratch/extract.log")
root="$scratch/image/squashfs-root"
test -f "$root/usr/lib/Rec EOS Launcher/libsteam_api.so"
test -f "$root/usr/lib/Rec EOS Launcher/receos-proton-helper.exe"
# The bundled libraries are found through usr/lib, as AppRun sets it up.
LD_LIBRARY_PATH="$root/usr/lib" ldd "$root/usr/bin/rec-eos-launcher" > "$scratch/ldd.log"
cat "$scratch/ldd.log"
! grep -F 'not found' "$scratch/ldd.log"
# libwayland must come from the host (scripts/appimage-drop-wayland.sh).
! ls "$root"/usr/lib/libwayland-*.so* 2>/dev/null
export XDG_CONFIG_HOME="$scratch/config"
export XDG_DATA_HOME="$scratch/data"
export XDG_CACHE_HOME="$scratch/cache"
export WEBKIT_DISABLE_COMPOSITING_MODE=1
# Only the isolated root-run Docker smoke test needs this WebKit override.
if [ "$(id -u)" = 0 ]; then export WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1; fi
set +e
xvfb-run -a dbus-run-session -- timeout --kill-after=5s 20s env "APPDIR=$root" "APPIMAGE=$image" "$root/AppRun" > "$scratch/image.log" 2>&1
result=$?
set -e
cat "$scratch/image.log"
test "$result" = 124
! grep -E 'panicked at|error while loading shared libraries|EGL_BAD_PARAMETER' "$scratch/image.log"
# This line is written after loadConfig succeeds, proving frontend IPC and
# JavaScript initialization, rather than just a surviving native process.
grep -R -F 'Launcher version:' "$XDG_DATA_HOME"
echo 'AppImage native linking and frontend startup passed.'
