#!/usr/bin/env bash
# Type-check the Windows and macOS builds from Linux (run via `mise run check:cross`).
# zig stands in as the C compiler that aws-lc-sys' build script needs, and a stub
# windres satisfies the Windows resource build. Nothing is linked or run.
#   Windows: every crate (x86_64-pc-windows-gnu)
#   macOS:   surge-core, surge-host, surge-cli (gpui's Metal shaders need Xcode,
#            so the GUI can only be checked on a Mac or in CI)
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$root/target/cross}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# The cc crate passes Rust-style --target=<triple>, which zig can't parse, so
# the wrappers drop it and give zig its own -target.
wrap() { # wrap <name> <zig target>
  cat > "$tmp/$1" <<SH
#!/bin/sh
for a in "\$@"; do shift; case "\$a" in --target=*) ;; *) set -- "\$@" "\$a" ;; esac; done
exec zig cc -target $2 "\$@"
SH
  chmod +x "$tmp/$1"
}
wrap cc-win x86_64-windows-gnu
wrap cc-mac-x64 x86_64-macos
wrap cc-mac-arm aarch64-macos
printf '#!/bin/sh\nexec zig ar "$@"\n' > "$tmp/ar"
chmod +x "$tmp/ar"
printf '#!/bin/sh\nexit 0\n' > "$tmp/x86_64-w64-mingw32-windres"
chmod +x "$tmp/x86_64-w64-mingw32-windres"
export PATH="$tmp:$PATH"

targets=$(rustc --print target-list >/dev/null; rustup target list --installed 2>/dev/null || true)
for t in x86_64-pc-windows-gnu x86_64-apple-darwin aarch64-apple-darwin; do
  grep -qx "$t" <<<"$targets" || rustup target add "$t"
done

echo "==> windows (x86_64-pc-windows-gnu), all crates"
CC_x86_64_pc_windows_gnu=cc-win AR_x86_64_pc_windows_gnu=ar \
  cargo clippy --locked --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings

for pair in x86_64-apple-darwin:cc-mac-x64 aarch64-apple-darwin:cc-mac-arm; do
  t=${pair%%:*}
  cc=${pair##*:}
  tu=${t//-/_}
  echo "==> macOS ($t), core/host/cli"
  env "CC_$tu=$cc" "AR_$tu=ar" \
    cargo clippy --locked -p surge-core -p surge-host -p surge-cli --all-targets --target "$t" -- -D warnings
done
echo "cross checks passed"
