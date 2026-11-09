#!/usr/bin/env python3
"""Download a signed version of the Hurtle add-on from AMO, if it's ready.

    scripts/amo-signed.py [version]   (default: the version in extension/dist/firefox)

`web-ext sign --approval-timeout 0` only uploads a version. AMO's review can
take minutes to hours, so this asks once and returns instead of waiting:
exit 0 when extension/dist/hurtle-firefox-<version>.xpi was written, 2 while
it is still waiting for approval, 1 on errors. The keys are AMO_JWT_ISSUER /
AMO_JWT_SECRET, from the environment or else from .env.local.
API: https://mozilla.github.io/addons-server/topics/api/addons.html
"""
import base64, hashlib, hmac, json, os, secrets, sys, time, urllib.error, urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DIST = ROOT / "extension" / "dist"


def key(name):
    if os.environ.get(name):
        return os.environ[name]
    env = ROOT / ".env.local"
    if env.exists():
        for line in env.read_text().splitlines():
            k, _, v = line.strip().removeprefix("export ").partition("=")
            if k.strip() == name:
                return v.strip().strip("\"'")
    sys.exit(f"{name} is not set (environment or .env.local)")


def b64(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=")


def token():
    now = int(time.time())
    claims = {"iss": key("AMO_JWT_ISSUER"), "jti": secrets.token_hex(16), "iat": now, "exp": now + 60}
    msg = b64(b'{"alg":"HS256","typ":"JWT"}') + b"." + b64(json.dumps(claims).encode())
    sig = hmac.new(key("AMO_JWT_SECRET").encode(), msg, hashlib.sha256).digest()
    return (msg + b"." + b64(sig)).decode()


def get(url):
    req = urllib.request.Request(url, headers={"Authorization": f"JWT {token()}", "User-Agent": "hurtle-build"})
    return urllib.request.urlopen(req, timeout=60)


def main():
    manifest = json.loads((DIST / "firefox" / "manifest.json").read_text())
    guid = manifest["browser_specific_settings"]["gecko"]["id"]
    version = sys.argv[1] if len(sys.argv) > 1 else manifest["version"]
    try:
        with get(f"https://addons.mozilla.org/api/v5/addons/addon/{guid}/versions/v{version}/") as r:
            info = json.load(r)
    except urllib.error.HTTPError as e:
        sys.exit(f"AMO has no version {version} of {guid} ({e.code}); was it submitted?")
    status = info["file"]["status"]
    if status != "public":
        print(f"{version}: not signed yet (AMO status: {status}). Run `mise run ext:fetch-signed` later.")
        return 2
    out = DIST / f"hurtle-firefox-{version}.xpi"
    with get(info["file"]["url"]) as r:
        data = r.read()
    algo, _, want = info["file"]["hash"].partition(":")
    if want and hashlib.new(algo, data).hexdigest() != want:
        sys.exit("downloaded xpi doesn't match AMO's hash")
    out.write_bytes(data)
    print(f"signed: {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
