#!/usr/bin/env bash
# Package a release build for one target into dist/:
#   Linux    surge-<ver>-linux-x86_64.tar.gz    (surge, surge-host, surge-cli)
#   Windows  surge-<ver>-windows-x86_64.zip     (surge.exe, surge-host.exe, surge-cli.exe)
#   macOS    surge-<ver>-macos-<arch>.zip       (Surge.app, the host inside it)
#
#   cargo build --release --target <triple> -p surge-app -p surge-host -p surge-cli
#   scripts/package.sh <triple>
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
target=${1:?usage: package.sh <target-triple>}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
bin=$root/target/$target/release
out=$root/dist
mkdir -p "$out"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

case $target in
  *-linux-*) os=linux ;;
  *-windows-*) os=windows ;;
  *-apple-darwin) os=macos ;;
  *) echo "unknown target $target" >&2; exit 1 ;;
esac
arch=${target%%-*}
name=surge-$version-$os-$arch

readme() {
  cat <<TXT
Surge $version — multi-connection download manager

1. Start Surge once. It registers itself with Chrome, Chromium, Brave, Edge,
   Vivaldi and Firefox so the browser extension can reach it.
   (To redo that by hand: surge-host --register; to undo: surge-host --unregister)
2. Install the browser extension from the same release page:
   - Chrome/Brave/Edge: unzip surge-chrome-$version.zip, open chrome://extensions,
     turn on Developer mode, "Load unpacked", pick the unzipped folder.
   - Firefox: install the .xpi (signed builds), or about:debugging →
     "Load Temporary Add-on" → manifest.json from surge-firefox-$version.zip.
3. Video sites and HLS/DASH streams need yt-dlp, ffmpeg and deno:
TXT
  case $os in
    windows) echo "     winget install yt-dlp.yt-dlp Gyan.FFmpeg DenoLand.Deno   (or put yt-dlp.exe next to surge.exe)" ;;
    macos) echo "     brew install yt-dlp ffmpeg deno" ;;
    linux) echo "     install yt-dlp, ffmpeg and deno with your package manager" ;;
  esac
  if [[ $os == macos ]]; then
    cat <<'TXT'

The app is not notarized. If macOS says it is damaged or can't be opened:
     xattr -dr com.apple.quarantine /Applications/Surge.app
TXT
  fi
}

case $os in
  linux)
    mkdir "$stage/$name"
    for b in surge surge-host surge-cli; do install -m 755 "$bin/$b" "$stage/$name/$b"; done
    install -m 644 "$root/extension/src/icons/icon-128.png" "$stage/$name/surge.png"
    readme > "$stage/$name/README.txt"
    tar -C "$stage" -czf "$out/$name.tar.gz" "$name"
    echo "$out/$name.tar.gz"
    ;;
  windows)
    mkdir "$stage/$name"
    for b in surge surge-host surge-cli; do cp "$bin/$b.exe" "$stage/$name/"; done
    readme | sed 's/$/\r/' > "$stage/$name/README.txt"
    (cd "$stage" && python -I -m zipfile -c "$out/$name.zip" "$name")
    echo "$out/$name.zip"
    ;;
  macos)
    app=$stage/Surge.app
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    # The host sits next to the app binary, which is where Surge looks for it.
    for b in surge surge-host surge-cli; do install -m 755 "$bin/$b" "$app/Contents/MacOS/$b"; done
    iconset=$stage/surge.iconset
    mkdir "$iconset"
    for s in 16 32 128 256 512; do
      sips -z $s $s "$root/extension/src/icons/icon-128.png" --out "$iconset/icon_${s}x${s}.png" >/dev/null
      sips -z $((s * 2)) $((s * 2)) "$root/extension/src/icons/icon-128.png" --out "$iconset/icon_${s}x${s}@2x.png" >/dev/null
    done
    iconutil -c icns "$iconset" -o "$app/Contents/Resources/surge.icns"
    cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Surge</string>
  <key>CFBundleDisplayName</key><string>Surge</string>
  <key>CFBundleIdentifier</key><string>com.surge.downloader</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleExecutable</key><string>surge</string>
  <key>CFBundleIconFile</key><string>surge</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
    # Ad-hoc signature (no Apple ID needed); Apple Silicon refuses unsigned code.
    codesign --force --deep --sign - "$app"
    mkdir "$stage/$name"
    mv "$app" "$stage/$name/"
    readme > "$stage/$name/README.txt"
    (cd "$stage" && ditto -c -k --sequesterRsrc --keepParent "$name" "$out/$name.zip")
    echo "$out/$name.zip"
    ;;
esac
