#!/usr/bin/env bash
# Exercise installed layouts, library loading, and frontend startup without
# Steam/accounts. Gameplay and desktop integration still need the manual matrix.
set -euo pipefail
deb=$(realpath "${1:?Usage: smoke-linux.sh PACKAGE.deb PACKAGE.AppImage}")
image=$(realpath "${2:?Usage: smoke-linux.sh PACKAGE.deb PACKAGE.AppImage}")
scratch=$(mktemp -d -t receos-smoke-XXXXXXXX)
dpkg-deb --info "$deb"
dpkg-deb --extract "$deb" "$scratch/deb"
test -f "$scratch/deb/usr/lib/Rec EOS Launcher/libsteam_api.so"
test -f "$scratch/deb/usr/lib/Rec EOS Launcher/receos-proton-helper.exe"
ldd "$scratch/deb/usr/bin/rec-eos-launcher" > "$scratch/ldd.log"
cat "$scratch/ldd.log"
! grep -F 'not found' "$scratch/ldd.log"
mkdir "$scratch/image"
(cd "$scratch/image" && env -u APPIMAGE_EXTRACT_AND_RUN "$image" --appimage-extract > "$scratch/extract.log")
test -f "$scratch/image/squashfs-root/usr/lib/Rec EOS Launcher/receos-proton-helper.exe"
for format in deb image; do
  if [ "$format" = deb ]; then launch=("$scratch/deb/usr/bin/rec-eos-launcher");
  else launch=(env "APPDIR=$scratch/image/squashfs-root" "APPIMAGE=$image" "$scratch/image/squashfs-root/AppRun"); fi
  export XDG_CONFIG_HOME="$scratch/$format-config"
  export XDG_DATA_HOME="$scratch/$format-data"
  export XDG_CACHE_HOME="$scratch/$format-cache"
  export WEBKIT_DISABLE_COMPOSITING_MODE=1
  # Only the isolated root-run Docker smoke test needs this WebKit override.
  if [ "$(id -u)" = 0 ]; then export WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1; fi
  set +e
  xvfb-run -a dbus-run-session -- timeout --kill-after=5s 20s "${launch[@]}" > "$scratch/$format.log" 2>&1
  result=$?
  set -e
  cat "$scratch/$format.log"
  test "$result" = 124
  ! grep -E 'panicked at|error while loading shared libraries' "$scratch/$format.log"
  # This line is written after loadConfig succeeds, proving frontend IPC and
  # JavaScript initialization, rather than just a surviving native process.
  grep -R -F 'Launcher version:' "$XDG_DATA_HOME"
done
echo 'Deb and AppImage native linking and frontend startup passed.'
