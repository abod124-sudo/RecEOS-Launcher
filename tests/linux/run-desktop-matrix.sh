#!/usr/bin/env bash
set -euo pipefail
output=${1:-/tmp/receos-desktop-results}
mkdir -p "$output" "$HOME"
for format in deb appimage; do
  if [ "$format" = deb ]; then application=/usr/bin/rec-eos-launcher; else application=/tools/receos-appimage; fi
  for display in x11 wayland; do
    status=0
    bash tests/linux/desktop.sh "$display" "$application" "$output/$format-$display" > "$output/$format-$display.log" 2>&1 || status=$?
    cat "$output/$format-$display.log"
    if [ "$status" != 0 ]; then exit "$status"; fi
  done
done
export WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1 WEBKIT_DISABLE_COMPOSITING_MODE=1 GDK_BACKEND=x11
status=0
xvfb-run -a dbus-run-session -- python3 tests/linux/no_tray_host.py > "$output/no-tray-host.log" 2>&1 || status=$?
cat "$output/no-tray-host.log"
exit "$status"
