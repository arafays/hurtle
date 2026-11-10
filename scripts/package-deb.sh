#!/usr/bin/env bash
# Build dist/hurtle_<ver>_<amd64|arm64>.deb from a release build, with ar + tar
# only (no dpkg-deb needed):
#   cargo build --release --target <triple> -p hurtle-app -p hurtle-host -p hurtle-cli
#   scripts/package-deb.sh <linux-triple>
# BIN_DIR overrides the directory holding the three binaries.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
target=${1:?usage: package-deb.sh <linux-target-triple>}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
case ${target%%-*} in
  x86_64) arch=amd64 ;;
  aarch64) arch=arm64 ;;
  *) echo "unsupported target $target" >&2; exit 1 ;;
esac
case $target in *-linux-*) ;; *) echo "not a Linux target: $target" >&2; exit 1 ;; esac
bin=${BIN_DIR:-$root/target/$target/release}
out=${OUT_DIR:-$root/dist}
mkdir -p "$out"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
d=$stage/data

install -d "$d/usr/bin" "$d/usr/share/applications" "$d/usr/share/icons/hicolor/128x128/apps" "$d/usr/share/doc/hurtle"
for b in hurtle hurtle-host hurtle-cli; do install -m 755 "$bin/$b" "$d/usr/bin/$b"; done
install -m 644 "$root/packaging/linux/hurtle.desktop" "$d/usr/share/applications/hurtle.desktop"
install -m 644 "$root/extension/src/icons/icon-128.png" "$d/usr/share/icons/hicolor/128x128/apps/hurtle.png"
install -m 644 "$root/LICENSE" "$d/usr/share/doc/hurtle/copyright"

# System-wide native-messaging manifests (host name and ids as in nmh.rs).
host=io.github.arafays.hurtle
chrome_id=llabdeoinnakkcncicakgafkjkkjfdgh
firefox_id=hurtle@arafays.github.io
chrome_json() {
  cat <<JSON
{
  "name": "$host",
  "description": "Hurtle download manager",
  "path": "/usr/bin/hurtle-host",
  "type": "stdio",
  "allowed_origins": [
    "chrome-extension://$chrome_id/"
  ]
}
JSON
}
firefox_json() {
  cat <<JSON
{
  "name": "$host",
  "description": "Hurtle download manager",
  "path": "/usr/bin/hurtle-host",
  "type": "stdio",
  "allowed_extensions": [
    "$firefox_id"
  ]
}
JSON
}
# Google Chrome, Chromium (also read by Brave, Vivaldi, Opera: they are built
# without Chrome branding), Microsoft Edge.
for dir in etc/opt/chrome etc/chromium etc/opt/edge; do
  install -d "$d/$dir/native-messaging-hosts"
  chrome_json > "$d/$dir/native-messaging-hosts/$host.json"
done
install -d "$d/usr/lib/mozilla/native-messaging-hosts"
firefox_json > "$d/usr/lib/mozilla/native-messaging-hosts/$host.json"
find "$d" -name '*.json' -exec chmod 644 {} +

size=$(du -sk "$d" | cut -f1)
install -d "$stage/control"
cat > "$stage/control/control" <<CTRL
Package: hurtle
Version: $version
Section: net
Priority: optional
Architecture: $arch
Maintainer: Hurtle <noreply@users.noreply.github.com>
Installed-Size: $size
Depends: libc6, libxkbcommon0, libvulkan1
Recommends: ffmpeg, yt-dlp
Homepage: https://github.com/arafays/hurtle
Description: Multi-connection download manager
 Hurtle downloads files as parallel byte-range segments, hands video sites
 and HLS/DASH streams to yt-dlp, and talks to the Hurtle browser extension
 through a native-messaging host (system manifests for Chrome, Chromium,
 Brave, Edge, Vivaldi and Firefox are installed).
CTRL
# md5sums
(cd "$d" && find . -type f -printf '%P\0' | sort -z | xargs -0 md5sum) > "$stage/control/md5sums"

tar_opts=(--owner=0 --group=0 --numeric-owner --sort=name --mtime=@"${SOURCE_DATE_EPOCH:-0}")
tar "${tar_opts[@]}" -C "$stage/control" -czf "$stage/control.tar.gz" ./control ./md5sums
tar "${tar_opts[@]}" -C "$d" -czf "$stage/data.tar.gz" .
printf '2.0\n' > "$stage/debian-binary"

deb=$out/hurtle_${version}_$arch.deb
rm -f "$deb"
(cd "$stage" && ar rc "$deb" debian-binary control.tar.gz data.tar.gz)
echo "$deb"
