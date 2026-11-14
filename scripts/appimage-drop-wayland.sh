#!/usr/bin/env bash
# Take the bundled libwayland-* back out of the AppImage, so it uses the host's.
#
# linuxdeploy copies the build machine's (Ubuntu 22.04) libwayland-client and
# friends into the AppImage, but the GL driver (libEGL / Mesa) always comes
# from the host. A newer Mesa loaded next to the old libwayland fails with
# "Could not create default EGL display: EGL_BAD_PARAMETER" and WebKit's web
# process aborts, leaving a blank window. Measured 2026-11-14 on Arch in CI:
# the stock AppImage aborted; the same AppDir without libwayland-*, or with the
# host's libwayland preloaded, started cleanly. Every desktop with GTK 3 has
# libwayland installed, so nothing is lost by leaving it to the host.
#
# The AppImage is rebuilt from its own runtime (the ELF in front of the
# squashfs) and the same compression, so no AppImage tool has to be fetched.
# Needs squashfs-tools. Nothing signs the Linux AppImage, so repacking it
# invalidates no signature.
set -euo pipefail
image=$(realpath "${1:?Usage: appimage-drop-wayland.sh PACKAGE.AppImage}")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

offset=$(env -u APPIMAGE_EXTRACT_AND_RUN "$image" --appimage-offset)
compression=$(unsquashfs -s -o "$offset" "$image" | sed -n 's/^Compression //p')
test -n "$compression"
unsquashfs -q -n -d "$work/root" -o "$offset" "$image" >/dev/null

removed=$(find "$work/root/usr/lib" -maxdepth 1 -name 'libwayland-*.so*' -print -delete)
if [ -z "$removed" ]; then
  echo "appimage-drop-wayland: no bundled libwayland in $(basename "$image"), left as is."
  exit 0
fi
echo "appimage-drop-wayland: removed $(echo "$removed" | xargs -n1 basename | tr '\n' ' ')"

head -c "$offset" "$image" > "$work/new.AppImage"
mksquashfs "$work/root" "$work/fs.squashfs" -comp "$compression" -all-root -noappend -no-progress -quiet
cat "$work/fs.squashfs" >> "$work/new.AppImage"
chmod 755 "$work/new.AppImage"
mv "$work/new.AppImage" "$image"
