#!/usr/bin/env bash
# Add a signed Firefox build to docs/updates.json, the update manifest the
# add-on's `update_url` points at (served by GitHub Pages from docs/). Firefox
# polls it about once a day and installs the newest listed version.
#   scripts/firefox-update.sh <version> <signed.xpi>
# The link is the xpi on that version's GitHub release. Re-running for a
# version replaces its entry; entries are kept newest first.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
version=${1:?version}
xpi=${2:?signed xpi}
unzip -l "$xpi" META-INF/mozilla.rsa >/dev/null || { echo "::error::$xpi is not signed" >&2; exit 1; }
hash=$(sha256sum "$xpi" | cut -d' ' -f1)
link="https://github.com/arafays/hurtle/releases/download/v$version/hurtle-firefox-$version.xpi"

python3 -I - "$root/docs/updates.json" "$version" "$link" "$hash" <<'PY'
import json, sys
path, version, link, digest = sys.argv[1:]
with open(path) as f:
    doc = json.load(f)
updates = doc["addons"]["hurtle@arafays.github.io"]["updates"]
updates[:] = [u for u in updates if u["version"] != version]
updates.append({"version": version, "update_link": link, "update_hash": f"sha256:{digest}"})
key = lambda u: [int(p) if p.isdigit() else 0 for p in u["version"].split(".")]
updates.sort(key=key, reverse=True)
with open(path, "w") as f:
    json.dump(doc, f, indent=2)
    f.write("\n")
PY
echo "docs/updates.json: $version -> $link"
