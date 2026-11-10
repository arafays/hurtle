#!/usr/bin/env bash
# Fill the package-manager templates (scoop, winget, Homebrew cask) for a release.
#   scripts/update-manifests.sh <version> [SHA256SUMS.txt] [out-dir]
# SHA256SUMS.txt is the release's checksum file (default dist/SHA256SUMS.txt); it must
# list the windows zip, the windows setup.exe and both macOS dmgs.
# Output goes to <out-dir> (default dist/manifests), ready to submit to a scoop bucket,
# microsoft/winget-pkgs and a Homebrew tap.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
version=${1:?usage: update-manifests.sh <version> [SHA256SUMS.txt] [out-dir]}
version=${version#v}
sums=${2:-$root/dist/SHA256SUMS.txt}
out=${3:-$root/dist/manifests}
[[ -f $sums ]] || { echo "$sums not found" >&2; exit 1; }

sha() {
  local file=$1 h
  h=$(awk -v f="$file" '{n=$2; sub(/^\*/, "", n)} n==f {print $1; exit}' "$sums")
  [[ -n $h ]] || { echo "$file not listed in $sums" >&2; exit 1; }
  echo "$h"
}
win=$(sha "hurtle-$version-windows-x86_64.zip")
setup=$(sha "hurtle-$version-windows-x86_64-setup.exe")
mac_arm=$(sha "hurtle-$version-macos-aarch64.dmg")
mac_x86=$(sha "hurtle-$version-macos-x86_64.dmg")

fill() {
  local src=$1 dst=$2
  mkdir -p "$(dirname "$dst")"
  sed -e "s/@VERSION@/$version/g" \
    -e "s/@SHA_WINDOWS@/$win/g" \
    -e "s/@SHA_WINDOWS_SETUP@/${setup^^}/g" \
    -e "s/@SHA_MACOS_AARCH64@/$mac_arm/g" \
    -e "s/@SHA_MACOS_X86_64@/$mac_x86/g" \
    "$src" > "$dst"
}

fill "$root/packaging/windows/scoop/hurtle.json" "$out/scoop/hurtle.json"
for f in "$root"/packaging/windows/winget/*.yaml; do
  fill "$f" "$out/winget/manifests/a/arafays/Hurtle/$version/$(basename "$f")"
done
fill "$root/packaging/macos/hurtle.rb" "$out/homebrew/Casks/hurtle.rb"
echo "$out"
