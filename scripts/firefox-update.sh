#!/usr/bin/env bash
# Add a signed Firefox build to docs/updates.json, the update manifest the
# add-on's `update_url` points at (served by GitHub Pages from docs/).
#   scripts/firefox-update.sh <version> <signed.xpi>
# Format: https://extensionworkshop.com/documentation/manage/updating-your-extension/
# Each entry links the xpi on that version's GitHub release, with its sha256,
# the release page as update_info_url, and the xpi's own strict_min_version.
# Re-running for a version replaces its entry; entries are kept newest first.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
version=${1:?version}
xpi=${2:?signed xpi}
unzip -l "$xpi" META-INF/mozilla.rsa >/dev/null || { echo "::error::$xpi is not signed" >&2; exit 1; }
hash=$(sha256sum "$xpi" | cut -d' ' -f1)
release="https://github.com/arafays/hurtle/releases"

unzip -p "$xpi" manifest.json | python3 -I -c '
import json, sys
path, version, release, digest = sys.argv[1:]
manifest = json.load(sys.stdin)
gecko = manifest["browser_specific_settings"]["gecko"]
got = manifest["version"]
if got != version:
    sys.exit(f"::error::xpi is version {got}, not {version}")
with open(path) as f:
    doc = json.load(f)
updates = doc["addons"][gecko["id"]]["updates"]  # KeyError: the xpi has another add-on id
entry = {
    "version": version,
    "update_link": f"{release}/download/v{version}/hurtle-firefox-{version}.xpi",
    "update_hash": f"sha256:{digest}",
    "update_info_url": f"{release}/tag/v{version}",
}
if "strict_min_version" in gecko:
    entry["applications"] = {"gecko": {"strict_min_version": gecko["strict_min_version"]}}
updates[:] = [u for u in updates if u["version"] != version] + [entry]
updates.sort(key=lambda u: [int(p) if p.isdigit() else 0 for p in u["version"].split(".")], reverse=True)
with open(path, "w") as f:
    json.dump(doc, f, indent=2)
    f.write("\n")
print("docs/updates.json:", version, "->", entry["update_link"])
' "$root/docs/updates.json" "$version" "$release" "$hash"
