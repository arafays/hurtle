#!/usr/bin/env bash
# Build dist/hurtle-<ver>-macos-<arch>.dmg from the zip scripts/package.sh made.
# macOS only (hdiutil/ditto). Run after: scripts/package.sh <triple>
#   scripts/package-dmg.sh aarch64-apple-darwin
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
target=${1:?usage: package-dmg.sh <apple-darwin-target-triple>}
[[ $target == *-apple-darwin ]] || { echo "package-dmg.sh needs a *-apple-darwin target, got $target" >&2; exit 1; }
if [[ $(uname -s) != Darwin ]] || ! command -v hdiutil >/dev/null; then
  echo "package-dmg.sh must run on macOS (needs hdiutil and ditto)" >&2
  exit 1
fi
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
arch=${target%%-*}
name=hurtle-$version-macos-$arch
out=${OUT_DIR:-$root/dist}
zip=$out/$name.zip
[[ -f $zip ]] || { echo "$zip not found; run scripts/package.sh $target first" >&2; exit 1; }

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
# ditto keeps the ad-hoc signature and extended attributes intact.
ditto -x -k "$zip" "$stage/unz"
app=$stage/unz/$name/Hurtle.app
[[ -d $app ]] || { echo "Hurtle.app not found inside $zip" >&2; exit 1; }
mkdir "$stage/dmg"
ditto "$app" "$stage/dmg/Hurtle.app"
ln -s /Applications "$stage/dmg/Applications"
cp "$stage/unz/$name/README.txt" "$stage/dmg/README.txt"

rm -f "$out/$name.dmg"
hdiutil create -volname "Hurtle $version" -srcfolder "$stage/dmg" -ov -format UDZO "$out/$name.dmg"
echo "$out/$name.dmg"
