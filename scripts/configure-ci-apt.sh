#!/usr/bin/env bash
# Configure reachable Ubuntu archives; apt still verifies Ubuntu signatures.
set -euo pipefail
. /etc/os-release
if [ "$ID" != ubuntu ]; then
  echo "Keeping $ID package sources."
  exit 0
fi

apt_scheme=http
if command -v curl >/dev/null && [ -s /etc/ssl/certs/ca-certificates.crt ]; then apt_scheme=https; fi
selected_archive=
for archive_host_path in archive.ubuntu.com/ubuntu mirrors.edge.kernel.org/ubuntu mirror.math.princeton.edu/pub/ubuntu; do
  archive_host=${archive_host_path%%/*}
  if ! timeout 5s getent ahostsv4 "$archive_host" >/dev/null; then
    echo "Ubuntu mirror DNS unavailable: $archive_host"
    continue
  fi
  candidate_archive="$apt_scheme://$archive_host_path"
  if command -v curl >/dev/null; then
    if ! timeout 15s curl --fail --silent --show-error --location --connect-timeout 5 --max-time 10 \
      --range 0-0 "$candidate_archive/dists/$VERSION_CODENAME/InRelease" >/dev/null; then
      echo "Ubuntu mirror unreachable: $archive_host"
      continue
    fi
  fi
  echo "Reachable Ubuntu archive: $candidate_archive"
  if [ -z "$selected_archive" ]; then selected_archive=$candidate_archive; fi
  if [ "${1:-}" != --probe-all ]; then break; fi
done
if [ -z "$selected_archive" ]; then
  echo 'No Ubuntu archive is reachable. Check the CI runner DNS/network.' >&2
  exit 1
fi
if [ "${1:-}" = --check ] || [ "${1:-}" = --probe-all ]; then exit 0; fi
if [ "$(id -u)" != 0 ]; then echo 'Run package-source configuration as root.' >&2; exit 1; fi

if [ -f /etc/apt/apt-mirrors.txt ]; then printf '%s\n' "$selected_archive" > /etc/apt/apt-mirrors.txt; fi
for source_file in /etc/apt/sources.list /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources; do
  if [ -f "$source_file" ]; then
    sed -i -E "s#https?://(azure\\.archive|archive|security)\\.ubuntu\\.com/ubuntu/?#$selected_archive#g" "$source_file"
  fi
done
