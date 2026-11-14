#!/usr/bin/env bash
# Temporary: why the AppImage aborts on Arch with EGL_BAD_PARAMETER.
# Runs as the test user; each variant gets 8 s (exit 124 = still alive).
set -u
deb=${1:?deb}
root=/opt/receos-validation/squashfs-root
work=$(mktemp -d)
export WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1 WEBKIT_DISABLE_COMPOSITING_MODE=1 GDK_BACKEND=x11
export XDG_CONFIG_HOME=$work/config XDG_DATA_HOME=$work/data XDG_CACHE_HOME=$work/cache
try() {
  local name=$1; shift
  local log=$work/$name.log
  timeout 8 xvfb-run -a dbus-run-session -- "$@" > "$log" 2>&1
  local code=$?
  local egl=no; grep -q EGL_BAD_PARAMETER "$log" && egl=yes
  echo "VARIANT $name: exit=$code egl_abort=$egl"
  grep -v '^\s*$' "$log" | head -6 | sed 's/^/    /'
}
cp -a "$root" "$work/b"; rm -f "$work"/b/usr/lib/libwayland-*.so*
cp -a "$work/b" "$work/b2"; rm -f "$work"/b2/usr/lib/libepoxy.so.0
mkdir -p "$work/deb" && bsdtar -xf "$deb" -C "$work/deb" && bsdtar -xf "$work"/deb/data.tar.* -C "$work/deb"
try A_control env APPDIR=$root APPIMAGE=/opt/receos-validation/x.AppImage $root/AppRun
try B_no_wayland env APPDIR=$work/b APPIMAGE=/opt/receos-validation/x.AppImage $work/b/AppRun
try B2_no_wayland_epoxy env APPDIR=$work/b2 APPIMAGE=/opt/receos-validation/x.AppImage $work/b2/AppRun
try C_preload_host_wayland env LD_PRELOAD=/usr/lib/libwayland-client.so.0:/usr/lib/libwayland-egl.so.1 APPDIR=$root APPIMAGE=/opt/receos-validation/x.AppImage $root/AppRun
try D_no_dmabuf env WEBKIT_DISABLE_DMABUF_RENDERER=1 APPDIR=$root APPIMAGE=/opt/receos-validation/x.AppImage $root/AppRun
try E_deb_system_libs "$work/deb/usr/bin/rec-eos-launcher"
exit 0
