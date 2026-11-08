#!/usr/bin/env bash
# Build Surge and install it for the current user:
#   ~/.local/bin/{surge,surge-host,surge-cli}, a desktop entry, and the
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
  rm -f "$HOME/.local/share/applications/surge.desktop" \
    "$HOME/.local/share/icons/hicolor/128x128/apps/surge.png"
  [[ -x $bin/surge-host ]] && "$bin/surge-host" --unregister | sed 's/^/    /'
  rm -f "$bin/surge" "$bin/surge-host" "$bin/surge-cli"
  echo "Surge uninstalled (downloads and ~/.local/share/surge were left alone)."
  exit 0
fi

for dep in yt-dlp ffmpeg; do
  command -v "$dep" >/dev/null || echo "note: $dep not found; video sites and HLS/DASH need it (pacman -S yt-dlp ffmpeg)"
done
command -v deno >/dev/null || command -v node >/dev/null ||
  echo "note: no JS runtime (deno) found; yt-dlp needs one for YouTube (pacman -S deno)"

echo "==> building (release)"
cargo build --release --manifest-path "$root/Cargo.toml" -p surge-app -p surge-host -p surge-cli

echo "==> installing binaries to $bin"
mkdir -p "$bin"
for b in surge surge-host surge-cli; do
  install -m 755 "$root/target/release/$b" "$bin/$b"
done

mkdir -p "$HOME/.local/share/applications" "$HOME/.local/share/icons/hicolor/128x128/apps"
install -m 644 "$root/extension/src/icons/icon-128.png" "$HOME/.local/share/icons/hicolor/128x128/apps/surge.png"
cat > "$HOME/.local/share/applications/surge.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Surge
GenericName=Download Manager
Comment=Multi-connection download accelerator
Exec=$bin/surge %U
Icon=surge
Categories=Network;FileTransfer;
StartupWMClass=surge
DESKTOP

echo "==> registering native-messaging host"
"$bin/surge-host" --register | sed 's/^/    /'

"$root/extension/build.sh"
cat <<MSG

Done. Load the extension:
  Chrome/Brave/Edge/Vivaldi: chrome://extensions → Developer mode → Load unpacked → $root/extension/dist/chrome
  Firefox:                   about:debugging#/runtime/this-firefox → Load Temporary Add-on → $root/extension/dist/firefox/manifest.json
  Zen/LibreWolf (permanent): about:config xpinstall.signatures.required = false,
                             then about:addons → ⚙ → Install Add-on From File → $root/extension/dist/surge-firefox-*-unsigned.xpi
Then start Surge from your launcher or run: surge
MSG
