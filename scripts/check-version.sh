#!/usr/bin/env bash
# Print the workspace version; with a tag argument (v1.2.3), fail unless it matches.
#   scripts/check-version.sh [v<version>]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
if [[ -n ${1:-} && ${1#v} != "$version" ]]; then
  echo "::error::tag $1 does not match Cargo.toml version $version" >&2
  exit 1
fi
echo "$version"
