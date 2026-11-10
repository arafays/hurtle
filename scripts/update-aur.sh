#!/usr/bin/env bash
# Point packaging/linux/aur/PKGBUILD at a release and refresh .SRCINFO.
#   scripts/update-aur.sh <version> [path/to/SHA256SUMS.txt]
# Without a sums file it is downloaded from the GitHub release.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
ver=${1:?usage: update-aur.sh <version> [SHA256SUMS.txt]}
ver=${ver#v}
dir=$root/packaging/linux/aur
sums=${2:-}
if [[ -z $sums ]]; then
  sums=$(mktemp)
  trap 'rm -f "$sums"' EXIT
  curl -fsSL -o "$sums" "https://github.com/arafays/hurtle/releases/download/v$ver/SHA256SUMS.txt"
fi
declare -A sum_of
for arch in x86_64 aarch64; do
  asset=hurtle-$ver-linux-$arch.tar.gz
  sum=$(awk -v f="$asset" '{ n = $2; sub(/^\*/, "", n); if (n == f) { print $1; exit } }' "$sums")
  [[ $sum =~ ^[0-9a-f]{64}$ ]] || { echo "no sha256 for $asset in the sums file" >&2; exit 1; }
  sum_of[$arch]=$sum
done
sed -i "s/^pkgver=.*/pkgver=$ver/; s/^pkgrel=.*/pkgrel=1/" "$dir/PKGBUILD"
for arch in x86_64 aarch64; do
  sed -i "s/^sha256sums_$arch=.*/sha256sums_$arch=('${sum_of[$arch]}')/" "$dir/PKGBUILD"
done
(cd "$dir" && makepkg --printsrcinfo > .SRCINFO)
echo "updated $dir to $ver"
