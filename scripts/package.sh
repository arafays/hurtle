#!/usr/bin/env bash
# Package a release build for one target into dist/:
#   Linux    hurtle-<ver>-linux-x86_64.tar.gz    (hurtle, hurtle-host, hurtle-cli, hurtle.desktop, install.sh)
#   Windows  hurtle-<ver>-windows-x86_64.zip     (hurtle.exe, hurtle-host.exe, hurtle-cli.exe)
#   macOS    hurtle-<ver>-macos-<arch>.zip       (Hurtle.app, the host inside it)
#
#   cargo build --release --target <triple> -p hurtle-app -p hurtle-host -p hurtle-cli
#   scripts/package.sh <triple>
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
target=${1:?usage: package.sh <target-triple>}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
bin=${BIN_DIR:-$root/target/$target/release}
out=${OUT_DIR:-$root/dist}
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
name=hurtle-$version-$os-$arch

readme() {
  cat <<TXT
Hurtle $version — multi-connection download manager

1. Start Hurtle once. It registers itself with Chrome, Chromium, Brave, Edge,
   Vivaldi and Firefox so the browser extension can reach it.
   (To redo that by hand: hurtle-host --register; to undo: hurtle-host --unregister)
2. Install the browser extension from the same release page:
   - Chrome/Brave/Edge: unzip hurtle-chrome-$version.zip, open chrome://extensions,
     turn on Developer mode, "Load unpacked", pick the unzipped folder.
   - Firefox: install the .xpi (signed builds), or about:debugging →
     "Load Temporary Add-on" → manifest.json from hurtle-firefox-$version.zip.
3. Video sites and HLS/DASH streams use yt-dlp, ffmpeg and deno. Installed
   copies are used; otherwise Hurtle fetches them the first time you save a
   video (official releases, SHA-256 checked) and keeps yt-dlp updated.
TXT
  if [[ $os == macos ]]; then
    cat <<'TXT'

The app is not notarized. If macOS says it is damaged or can't be opened:
     xattr -dr com.apple.quarantine /Applications/Hurtle.app
TXT
  fi
}

case $os in
  linux)
    mkdir "$stage/$name"
    for b in hurtle hurtle-host hurtle-cli; do install -m 755 "$bin/$b" "$stage/$name/$b"; done
    install -m 644 "$root/extension/src/icons/icon-128.png" "$stage/$name/hurtle.png"
    install -m 644 "$root/packaging/linux/hurtle.desktop" "$stage/$name/hurtle.desktop"
    install -m 644 "$root/LICENSE" "$stage/$name/LICENSE"
    install -m 755 "$root/packaging/linux/install.sh" "$stage/$name/install.sh"
    readme > "$stage/$name/README.txt"
    printf '\n4. Optional: ./install.sh puts Hurtle in ~/.local (launcher entry, icon, browser host);\n   ./install.sh --uninstall removes it.\n' >> "$stage/$name/README.txt"
    tar -C "$stage" -czf "$out/$name.tar.gz" "$name"
    echo "$out/$name.tar.gz"
    ;;
  windows)
    mkdir "$stage/$name"
    for b in hurtle hurtle-host hurtle-cli; do cp "$bin/$b.exe" "$stage/$name/"; done
    readme | sed 's/$/\r/' > "$stage/$name/README.txt"
    (cd "$stage" && python -I -m zipfile -c "$out/$name.zip" "$name")
    echo "$out/$name.zip"
    ;;
  macos)
    app=$stage/Hurtle.app
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    # The host sits next to the app binary, which is where Hurtle looks for it.
    for b in hurtle hurtle-host hurtle-cli; do install -m 755 "$bin/$b" "$app/Contents/MacOS/$b"; done
    iconset=$stage/hurtle.iconset
    mkdir "$iconset"
    for s in 16 32 128 256 512; do
      sips -z $s $s "$root/extension/src/icons/icon-128.png" --out "$iconset/icon_${s}x${s}.png" >/dev/null
      sips -z $((s * 2)) $((s * 2)) "$root/extension/src/icons/icon-128.png" --out "$iconset/icon_${s}x${s}@2x.png" >/dev/null
    done
    iconutil -c icns "$iconset" -o "$app/Contents/Resources/hurtle.icns"
    cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Hurtle</string>
  <key>CFBundleDisplayName</key><string>Hurtle</string>
  <key>CFBundleIdentifier</key><string>io.github.arafays.hurtle</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleExecutable</key><string>hurtle</string>
  <key>CFBundleIconFile</key><string>hurtle</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <!-- Offered for magnet: links and .torrent files (Hurtle's settings can make it the default). -->
  <key>CFBundleURLTypes</key>
  <array>
    <dict>
      <key>CFBundleURLName</key><string>Magnet link</string>
      <key>CFBundleURLSchemes</key><array><string>magnet</string></array>
      <key>CFBundleTypeRole</key><string>Viewer</string>
    </dict>
  </array>
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key><string>BitTorrent file</string>
      <key>CFBundleTypeRole</key><string>Viewer</string>
      <key>LSHandlerRank</key><string>Default</string>
      <key>LSItemContentTypes</key><array><string>org.bittorrent.torrent</string></array>
      <key>CFBundleTypeExtensions</key><array><string>torrent</string></array>
    </dict>
  </array>
  <key>UTImportedTypeDeclarations</key>
  <array>
    <dict>
      <key>UTTypeIdentifier</key><string>org.bittorrent.torrent</string>
      <key>UTTypeDescription</key><string>BitTorrent file</string>
      <key>UTTypeConformsTo</key><array><string>public.data</string></array>
      <key>UTTypeTagSpecification</key>
      <dict>
        <key>public.filename-extension</key><array><string>torrent</string></array>
        <key>public.mime-type</key><array><string>application/x-bittorrent</string></array>
      </dict>
    </dict>
  </array>
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
