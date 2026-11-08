#!/usr/bin/env bash
# Build the extension for Chrome-family browsers and Firefox:
#   extension/dist/chrome   (load unpacked in chrome://extensions)
#   extension/dist/firefox  (about:debugging → Load Temporary Add-on; it is gone
#                            after a browser restart, so install the .xpi instead)
# plus .zip packages next to them.
set -euo pipefail
cd "$(dirname "$0")"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' ../Cargo.toml | head -1)
# Local builds get a 4th version part (day of year and quarter hour, UTC), so
# reinstalling a rebuilt add-on is an update: Firefox and Zen otherwise keep
# serving the old files of a same-version reinstall. Release builds (CI) don't.
manifest_version=$version
if [ -z "${CI:-}" ]; then
  manifest_version="$version.$((10#$(date -u +%j) * 100 + 10#$(date -u +%H) * 4 + 10#$(date -u +%M) / 15))"
fi
key=$(cat chrome-key.txt)

common() {
  cat <<JSON
  "manifest_version": 3,
  "name": "Hurtle Download Manager",
  "short_name": "Hurtle",
  "version": "$manifest_version",
  "description": "Send downloads to Hurtle: multi-connection, resumable downloads and video sites.",
  "homepage_url": "https://arafays.github.io/hurtle/",
  "icons": { "16": "icons/icon-16.png", "32": "icons/icon-32.png", "48": "icons/icon-48.png", "128": "icons/icon-128.png" },
  "action": { "default_popup": "popup.html", "default_title": "Hurtle", "default_icon": { "16": "icons/icon-16.png", "32": "icons/icon-32.png" } },
  "permissions": ["downloads", "nativeMessaging", "cookies", "storage", "contextMenus", "webRequest", "tabs"],
  "host_permissions": ["<all_urls>"],
  "content_scripts": [{ "matches": ["<all_urls>"], "js": ["mediaui.js", "content.js"], "all_frames": true, "run_at": "document_idle" }],
JSON
}

# Build next to dist/ and swap each folder in at once at the end: browsers
# load dist/chrome (unpacked) and dist/firefox (temporary add-on) straight
# from disk, and drop the extension if they find the folder gone mid-build.
out=dist/.build
rm -rf "$out" && mkdir -p "$out/chrome" "$out/firefox"
for target in chrome firefox; do
  cp -r src/. "$out/$target/"
  rm -f "$out/$target/icons/icon.svg"
done

{
  echo "{"
  common
  cat <<JSON
  "key": "$key",
  "minimum_chrome_version": "116",
  "background": { "service_worker": "background.js" }
}
JSON
} > "$out/chrome/manifest.json"

{
  echo "{"
  common
  cat <<'JSON'
  "background": { "scripts": ["background.js"] },
  "browser_specific_settings": { "gecko": { "id": "hurtle@arafays.github.io", "strict_min_version": "142.0", "update_url": "https://arafays.github.io/hurtle/updates.json", "data_collection_permissions": { "required": ["none"] } } }
}
JSON
} > "$out/firefox/manifest.json"

for target in chrome firefox; do
  python3 -I -c 'import json,sys; json.load(open(sys.argv[1]))' "$out/$target/manifest.json"
  (cd "$out/$target" && python3 -I -m zipfile -c "../hurtle-$target-$version.zip" .)
done
# The same Firefox zip as an .xpi, for "Install Add-on From File" in Firefox
# forks that allow unsigned add-ons (Zen, Librewolf, Developer Edition, Nightly).
cp "$out/hurtle-firefox-$version.zip" "$out/hurtle-firefox-$version-unsigned.xpi"

for target in chrome firefox; do
  if [ -d "dist/$target" ] && mv --exchange -T "$out/$target" "dist/$target" 2>/dev/null; then
    rm -rf "${out:?}/$target" # now the previous build
  else
    rm -rf "dist/$target" && mv -T "$out/$target" "dist/$target"
  fi
done
rm -f dist/*.zip dist/*.xpi
mv "$out"/*.zip "$out"/*.xpi dist/
rmdir "$out"
echo "built extension/dist/{chrome,firefox} (Chrome extension id: $(cat chrome-id.txt))"
