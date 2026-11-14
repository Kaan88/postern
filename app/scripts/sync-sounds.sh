#!/usr/bin/env bash
# Keeps app/sounds/ (Bulwark's notification sounds) in step with upstream.
#   bash app/scripts/sync-sounds.sh           download the current files into app/sounds/ and
#                                             write the upstream commit into sounds/README.md
#   bash app/scripts/sync-sounds.sh --check   change nothing; exit 0 if the files are the same as
#                                             upstream's, 1 if not (or if they can't be checked)
# Either way it also compares upstream's list of sound ids and files (lib/notification-sound.ts)
# with the one src/sound.rs knows, and says so when they differ: that part is edited by hand.
# Needs only curl; set GH_TOKEN (or GITHUB_TOKEN) to avoid GitHub's low anonymous API limit.
set -euo pipefail

repo=bulwarkmail/webmail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sounds="$root/sounds"
rust="$root/src/sound.rs"

check=
case "${1:-}" in
  "") ;;
  --check) check=1 ;;
  *) echo "usage: sync-sounds.sh [--check]" >&2; exit 2 ;;
esac

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

auth=()
token="${GH_TOKEN:-${GITHUB_TOKEN:-}}"
[ -z "$token" ] || auth=(-H "Authorization: Bearer $token")
fetch() { curl -fsSL --retry 2 --max-time 60 "$@"; }

# Pin one commit, so the list and the files below can't come from two different states.
sha="$(fetch ${auth[@]+"${auth[@]}"} -H 'Accept: application/vnd.github.sha' "https://api.github.com/repos/$repo/commits/main")"
[[ "$sha" =~ ^[0-9a-f]{40}$ ]] || { echo "unexpected answer for the upstream commit: $sha" >&2; exit 1; }
raw="https://raw.githubusercontent.com/$repo/$sha"

fetch "$raw/lib/notification-sound.ts" > "$work/notification-sound.ts"
# "<id> <file>" per sound, "-" for the synthesized one; file names without their directory.
sed -n "s/.*{ id: '\([a-z]*\)'\(, file: '[^']*\/\([^'\/]*\)'\)\{0,1\} }.*/\1 \3/p" "$work/notification-sound.ts" \
  | awk '{ print $1, ($2 == "" ? "-" : $2) }' | sort > "$work/upstream.list"
[ -s "$work/upstream.list" ] || { echo "could not read the sound list from lib/notification-sound.ts (format changed?)" >&2; exit 1; }

# What the Rust code knows: beep!("id") and mp3!("id", "file") lines in src/sound.rs.
{ sed -n 's/^[[:space:]]*beep!("\([a-z]*\)"),$/\1 -/p; s/^[[:space:]]*mp3!("\([a-z]*\)", "\([^"]*\)"),$/\1 \2/p' "$rust"; } | sort > "$work/rust.list"

status=0
if ! diff -u "$work/rust.list" "$work/upstream.list" > "$work/list.diff"; then
  echo "DIFFERS: upstream's sound list is not the one in src/sound.rs (- ours, + upstream):"
  tail -n +3 "$work/list.diff"
  echo "Update the SOUNDS table in src/sound.rs, the settings list in ui/settings.html and the README."
  status=1
fi

mkdir "$work/new"
while read -r id file; do
  [ "$file" != "-" ] || continue
  fetch "$raw/public/notification/$file" > "$work/new/$file"
  [ -s "$work/new/$file" ] || { echo "empty download: $file" >&2; exit 1; }
done < "$work/upstream.list"

# Compare with what is in app/sounds (only the audio files).
differs=
for f in "$work"/new/*; do
  name="$(basename "$f")"
  cmp -s "$f" "$sounds/$name" || { echo "DIFFERS: $name (upstream $sha)"; differs=1; }
done
for f in "$sounds"/*.mp3; do
  [ -f "$work/new/$(basename "$f")" ] || { echo "DIFFERS: $(basename "$f") is no longer in upstream's list"; differs=1; }
done
[ -z "$differs" ] || status=1

if [ -n "$check" ]; then
  [ "$status" -ne 0 ] || echo "same: app/sounds matches $repo at $sha"
  exit "$status"
fi

# Update: replace the files, then record where they were taken.
for f in "$sounds"/*.mp3; do [ -f "$work/new/$(basename "$f")" ] || rm -f "$f"; done
cp -f "$work"/new/* "$sounds/"
sed -i "s/^Upstream commit: .*/Upstream commit: $sha/" "$sounds/README.md"
echo "app/sounds updated from $repo at $sha. Review the diff and commit it."
