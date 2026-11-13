#!/usr/bin/env bash
# Builds Postern's Linux packages for the host architecture and lays them out like the Windows
# installers (see release.ps1):
#   Postern-<version>-<arch>.deb         Debian / Ubuntu
#   Postern-<version>-<arch>.rpm         Fedora / openSUSE
#   Postern-<version>-<arch>.AppImage    anything else (bundles the tray library)
#   Postern-<version>-<arch>.AppImage.sig  the AppImage's update signature (see below)
#   <file>.sha256                        `sha256sum` format
# <arch> is x64 or arm64, matching the Windows artifacts. Output goes to dist/<version>/.
# Usage (from anywhere):  bash app/scripts/release.sh [deb] [rpm] [appimage]   (default: all three)
# Bundle types may also be comma separated: release.sh deb,rpm
# Honours CARGO_TARGET_DIR (relative paths resolve against app/, where cargo runs).
# An AppImage updates itself from the release's latest.json, and only takes an update that is
# signed. Set TAURI_SIGNING_PRIVATE_KEY (the key or a path to it; TAURI_SIGNING_PRIVATE_KEY_PASSWORD
# if it has a password) to get the .sig; without it the AppImage is built but cannot be offered.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="$(sed -n 's/^[[:space:]]*"version":[[:space:]]*"\([^"]*\)".*/\1/p' "$root/tauri.conf.json" | head -n1)"
[ -n "$version" ] || { echo "could not read the version from tauri.conf.json" >&2; exit 1; }

case "$(uname -m)" in
  x86_64)        arch=x64 ;;
  aarch64|arm64) arch=arm64 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

wanted="${*:-deb rpm appimage}"
wanted="${wanted//,/ }"
bundles=()
for b in $wanted; do
  b="$(printf '%s' "$b" | tr '[:upper:]' '[:lower:]')"
  case "$b" in
    deb|rpm|appimage) bundles+=("$b") ;;
    *) echo "unknown bundle type: $b (use deb, rpm, appimage)" >&2; exit 1 ;;
  esac
done
[ "${#bundles[@]}" -gt 0 ] || { echo "no bundle types given" >&2; exit 1; }
bundle_list="$(IFS=,; echo "${bundles[*]}")"

dist="$root/dist/$version"
mkdir -p "$dist"

# linuxdeploy's bundled strip chokes on the newer ELF sections modern distros emit.
export NO_STRIP="${NO_STRIP:-true}"

# Not in tauri.linux.conf.json: asking for the update artifacts fails a build that has no key.
signed=
extra=()
for b in "${bundles[@]}"; do
  if [ "$b" = appimage ]; then
    if [ -n "${TAURI_SIGNING_PRIVATE_KEY:-}" ]; then
      signed=1
      extra=(--config '{"bundle":{"createUpdaterArtifacts":true}}')
    else
      echo "WARNING: TAURI_SIGNING_PRIVATE_KEY is not set: this AppImage cannot be offered as an update" >&2
    fi
  fi
done

echo "== $arch ($bundle_list)"
(cd "$root" && cargo tauri build --bundles "$bundle_list" ${extra[@]+"${extra[@]}"})

target_dir="${CARGO_TARGET_DIR:-$root/target}"
case "$target_dir" in /*) ;; *) target_dir="$root/$target_dir" ;; esac
bundle="$target_dir/release/bundle"

# newest file matching the glob in a directory (the bundler's own names carry the version)
newest() { # dir pattern
  local f
  f="$(ls -t "$1"/$2 2>/dev/null | head -n1 || true)"
  [ -n "$f" ] || { echo "no $2 found in $1" >&2; exit 1; }
  printf '%s\n' "$f"
}

for b in "${bundles[@]}"; do
  case "$b" in
    deb)      cp -f "$(newest "$bundle/deb" "*${version}*.deb")" "$dist/Postern-$version-$arch.deb" ;;
    rpm)      cp -f "$(newest "$bundle/rpm" "*${version}*.rpm")" "$dist/Postern-$version-$arch.rpm" ;;
    appimage)
      cp -f "$(newest "$bundle/appimage" "*${version}*.AppImage")" "$dist/Postern-$version-$arch.AppImage"
      # The signature covers the file's contents, not its name, so the renamed copy still verifies.
      [ -z "$signed" ] || cp -f "$(newest "$bundle/appimage" "*${version}*.AppImage.sig")" "$dist/Postern-$version-$arch.AppImage.sig"
      ;;
  esac
done

cd "$dist"
for f in Postern-"$version"-"$arch".deb Postern-"$version"-"$arch".rpm Postern-"$version"-"$arch".AppImage; do
  [ -f "$f" ] && sha256sum "$f" > "$f.sha256"
done
ls -l "$dist"
