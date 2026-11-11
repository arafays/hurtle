#!/bin/sh
# Hurtle installer for Linux and macOS (no sudo):
#   curl -fsSL https://arafays.github.io/hurtle/install.sh | sh
#   ... | sh -s -- --uninstall
# Environment:
#   HURTLE_VERSION   version to install (e.g. 0.2.2); default: latest release
#   HURTLE_PREFIX    Linux: install root instead of ~/.local
#   HURTLE_APPDIR    macOS: folder for Hurtle.app (default /Applications, else ~/Applications)
#   HURTLE_BASE_URL  release download base (for mirrors and tests)
set -eu

REPO=arafays/hurtle

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

fetch() { # url dest
  if have curl; then curl -fsSL -o "$2" "$1"
  elif have wget; then wget -q -O "$2" "$1"
  else die "need curl or wget"; fi
}

sha256_of() {
  if have sha256sum; then sha256sum "$1" | cut -d' ' -f1
  elif have shasum; then shasum -a 256 "$1" | cut -d' ' -f1
  else die "need sha256sum or shasum to verify the download"; fi
}

latest_version() {
  have curl || die "set HURTLE_VERSION (curl is needed to look up the latest release)"
  url=$(curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest") ||
    die "could not look up the latest release"
  tag=${url##*/}
  case $tag in v[0-9]*) printf '%s' "${tag#v}" ;; *) die "no release found at $url" ;; esac
}

macos_appdir() {
  if [ -n "${HURTLE_APPDIR:-}" ]; then printf '%s' "$HURTLE_APPDIR"
  elif [ -w /Applications ]; then printf '/Applications'
  else printf '%s/Applications' "$HOME"; fi
}

uninstall() {
  case $(uname -s) in
    Darwin)
      app=$(macos_appdir)/Hurtle.app
      [ -x "$app/Contents/MacOS/hurtle-host" ] && "$app/Contents/MacOS/hurtle-host" --unregister || true
      rm -rf "$app"
      ;;
    *)
      prefix=${HURTLE_PREFIX:-$HOME/.local}
      [ -x "$prefix/bin/hurtle-host" ] && "$prefix/bin/hurtle-host" --unregister || true
      rm -f "$prefix/bin/hurtle" "$prefix/bin/hurtle-host" "$prefix/bin/hurtle-cli" \
        "$prefix/share/applications/hurtle.desktop" "$prefix/share/icons/hicolor/128x128/apps/hurtle.png"
      ;;
  esac
  say "Hurtle removed. Downloads and settings were kept."
}

main() {
  [ "${1:-}" = "--uninstall" ] && { uninstall; return; }
  [ "$(id -u)" -ne 0 ] || die "run as your own user, not root"

  case $(uname -s) in Linux) os=linux ;; Darwin) os=macos ;; *) die "unsupported OS (Windows: use the zip from the releases page)" ;; esac
  case $(uname -m) in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) die "unsupported CPU $(uname -m)" ;;
  esac

  version=${HURTLE_VERSION:-$(latest_version)}
  version=${version#v}
  if [ "$os" = linux ]; then ext=tar.gz; else ext=zip; fi
  asset=hurtle-$version-$os-$arch.$ext
  base=${HURTLE_BASE_URL:-https://github.com/$REPO/releases/download/v$version}

  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT INT TERM
  say "==> downloading Hurtle $version ($os-$arch)"
  fetch "$base/$asset" "$tmp/$asset"
  fetch "$base/SHA256SUMS.txt" "$tmp/SHA256SUMS.txt"

  want=$(awk -v f="$asset" '{ n = $2; sub(/^\*/, "", n); if (n == f) { print $1; exit } }' "$tmp/SHA256SUMS.txt")
  [ -n "$want" ] || die "$asset is not listed in SHA256SUMS.txt"
  got=$(sha256_of "$tmp/$asset")
  [ "$want" = "$got" ] || die "checksum mismatch for $asset (expected $want, got $got)"
  say "==> checksum ok"

  mkdir "$tmp/x"
  if [ "$os" = linux ]; then
    have tar || die "need tar"
    tar -xzf "$tmp/$asset" -C "$tmp/x"
    src=$tmp/x/hurtle-$version-$os-$arch
    prefix=${HURTLE_PREFIX:-$HOME/.local}
    mkdir -p "$prefix/bin" "$prefix/share/applications" "$prefix/share/icons/hicolor/128x128/apps"
    for b in hurtle hurtle-host hurtle-cli; do install -m 755 "$src/$b" "$prefix/bin/$b"; done
    install -m 644 "$src/hurtle.png" "$prefix/share/icons/hicolor/128x128/apps/hurtle.png"
    if [ -f "$src/hurtle.desktop" ]; then
      sed "s|^Exec=hurtle |Exec=$prefix/bin/hurtle |" "$src/hurtle.desktop" > "$prefix/share/applications/hurtle.desktop"
    else # older releases ship no desktop entry
      printf '%s\n' '[Desktop Entry]' 'Type=Application' 'Name=Hurtle' 'GenericName=Download Manager' \
        'Comment=Multi-connection download accelerator' "Exec=$prefix/bin/hurtle %U" 'Icon=hurtle' \
        'Categories=Network;FileTransfer;P2P;' 'MimeType=x-scheme-handler/magnet;application/x-bittorrent;' 'StartupWMClass=hurtle' 'Terminal=false' \
        > "$prefix/share/applications/hurtle.desktop"
    fi
    have update-desktop-database && update-desktop-database "$prefix/share/applications" >/dev/null 2>&1 || true
    host=$prefix/bin/hurtle-host
    where=$prefix/bin
    case ":$PATH:" in *":$prefix/bin:"*) ;; *) say "note: add $prefix/bin to your PATH" ;; esac
  else
    if have ditto; then ditto -x -k "$tmp/$asset" "$tmp/x"
    else have unzip || die "need unzip"; unzip -q "$tmp/$asset" -d "$tmp/x"; fi
    appdir=$(macos_appdir)
    mkdir -p "$appdir"
    rm -rf "$appdir/Hurtle.app"
    cp -R "$tmp/x/hurtle-$version-$os-$arch/Hurtle.app" "$appdir/Hurtle.app"
    have xattr && xattr -dr com.apple.quarantine "$appdir/Hurtle.app" 2>/dev/null || true
    host=$appdir/Hurtle.app/Contents/MacOS/hurtle-host
    where=$appdir/Hurtle.app
  fi

  say "==> registering the browser host"
  "$host" --register

  cat <<TXT

Hurtle $version is installed in $where.

Next: install the browser extension (from https://github.com/$REPO/releases/tag/v$version):
  - Chrome/Brave/Edge: unzip hurtle-chrome-$version.zip, open chrome://extensions,
    turn on Developer mode, "Load unpacked", pick the folder.
  - Firefox: open the signed hurtle-firefox-$version.xpi.
Uninstall: curl -fsSL https://arafays.github.io/hurtle/install.sh | sh -s -- --uninstall
TXT
}

main "$@"
