#!/usr/bin/env bash
# Build Hurtle and install it for the current user:
#   ~/.local/bin/{hurtle,hurtle-host,hurtle-cli}, a desktop entry, and the
#   native-messaging host manifest for every Chrome-family / Firefox-family
#   browser config found under $HOME.
#
#   scripts/install.sh            install / update
#   scripts/install.sh --uninstall
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
prefix=${PREFIX:-$HOME/.local}
bin=$prefix/bin


if [[ ${1:-} == --uninstall ]]; then
  rm -f "$HOME/.local/share/applications/hurtle.desktop" \
    "$HOME/.local/share/icons/hicolor/128x128/apps/hurtle.png"
  [[ -x $bin/hurtle-host ]] && "$bin/hurtle-host" --unregister | sed 's/^/    /'
  rm -f "$bin/hurtle" "$bin/hurtle-host" "$bin/hurtle-cli"
  echo "Hurtle uninstalled (downloads and ~/.local/share/hurtle were left alone)."
  exit 0
fi

for dep in yt-dlp ffmpeg deno; do
  command -v "$dep" >/dev/null || echo "note: $dep not found; Hurtle fetches it on the first video download (or: pacman -S $dep)"
done

echo "==> building (release)"
cargo build --release --manifest-path "$root/Cargo.toml" -p hurtle-app -p hurtle-host -p hurtle-cli

echo "==> installing binaries to $bin"
mkdir -p "$bin"
for b in hurtle hurtle-host hurtle-cli; do
  install -m 755 "$root/target/release/$b" "$bin/$b"
done

# Hurtle was called Surge until 0.2: remove the old install. The data dir is
# moved on first start; the old host's browser manifests go when hurtle-host
# registers below.
if [[ -e $bin/surge || -e $bin/surge-host ]]; then
  echo "==> removing the old Surge install (quit Surge first if it is running)"
  rm -f "$bin/surge" "$bin/surge-host" "$bin/surge-cli" \
    "$HOME/.local/share/applications/surge.desktop" \
    "$HOME/.local/share/icons/hicolor/128x128/apps/surge.png"
fi

mkdir -p "$HOME/.local/share/applications" "$HOME/.local/share/icons/hicolor/128x128/apps"
install -m 644 "$root/extension/src/icons/icon-128.png" "$HOME/.local/share/icons/hicolor/128x128/apps/hurtle.png"
cat > "$HOME/.local/share/applications/hurtle.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Hurtle
GenericName=Download Manager
Comment=Multi-connection download accelerator
Exec=$bin/hurtle %U
Icon=hurtle
Categories=Network;FileTransfer;
StartupWMClass=hurtle
DESKTOP

echo "==> registering native-messaging host"
"$bin/hurtle-host" --register | sed 's/^/    /'

"$root/extension/build.sh"
cat <<MSG

Done. Load the extension:
  Chrome/Brave/Edge/Vivaldi: chrome://extensions → Developer mode → Load unpacked → $root/extension/dist/chrome
  Firefox:                   about:debugging#/runtime/this-firefox → Load Temporary Add-on → $root/extension/dist/firefox/manifest.json
  Zen/LibreWolf (permanent): about:config xpinstall.signatures.required = false,
                             then about:addons → ⚙ → Install Add-on From File → $root/extension/dist/hurtle-firefox-*-unsigned.xpi
Then start Hurtle from your launcher or run: hurtle
MSG
