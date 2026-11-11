#!/usr/bin/env bash
set -euo pipefail
deb=$(realpath "${1:?Deb package required}")
image=$(realpath "${2:?AppImage required}")
mkdir -p /tools/bin /opt/receos-validation
if [ ! -x /tools/bin/tauri-driver ]; then
  cp src-tauri/target/linux-validation/account-free/tauri-driver /tools/bin/tauri-driver
  chmod 755 /tools/bin/tauri-driver
fi
dpkg -i "$deb"
cp "$image" '/opt/receos-validation/Rec EOS Launcher.AppImage'
chmod 755 '/opt/receos-validation/Rec EOS Launcher.AppImage'
chown 1000:1000 /opt/receos-validation '/opt/receos-validation/Rec EOS Launcher.AppImage'
user=$(getent passwd 1000 | cut -d: -f1)
runuser -u "$user" -- sh -c 'cd /opt/receos-validation && env -u APPIMAGE_EXTRACT_AND_RUN "./Rec EOS Launcher.AppImage" --appimage-extract > extract.log'
cat > /tools/receos-appimage <<'EOF'
#!/bin/sh
export APPDIR=/opt/receos-validation/squashfs-root
export APPIMAGE='/opt/receos-validation/Rec EOS Launcher.AppImage'
exec "$APPDIR/AppRun" "$@"
EOF
chmod 755 /tools/receos-appimage
