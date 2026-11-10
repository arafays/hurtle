#!/bin/sh
# Install Hurtle from this extracted archive into ~/.local (no sudo).
#   ./install.sh            install
#   ./install.sh --uninstall
# HURTLE_PREFIX overrides ~/.local.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
prefix=${HURTLE_PREFIX:-$HOME/.local}
bin=$prefix/bin
apps=$prefix/share/applications
icons=$prefix/share/icons/hicolor/128x128/apps

if [ "${1:-}" = "--uninstall" ]; then
  [ -x "$bin/hurtle-host" ] && "$bin/hurtle-host" --unregister || true
  rm -f "$bin/hurtle" "$bin/hurtle-host" "$bin/hurtle-cli" "$apps/hurtle.desktop" "$icons/hurtle.png"
  echo "Hurtle removed. Downloads and settings in ~/.local/share/hurtle were kept."
  exit 0
fi

mkdir -p "$bin" "$apps" "$icons"
for b in hurtle hurtle-host hurtle-cli; do
  install -m 755 "$here/$b" "$bin/$b"
done
install -m 644 "$here/hurtle.png" "$icons/hurtle.png"
sed "s|^Exec=hurtle |Exec=$bin/hurtle |" "$here/hurtle.desktop" > "$apps/hurtle.desktop"
chmod 644 "$apps/hurtle.desktop"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$apps" >/dev/null 2>&1 || true
"$bin/hurtle-host" --register
echo "Installed to $bin. Make sure it is on your PATH, then install the browser extension (see README.txt)."
