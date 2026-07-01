#!/usr/bin/env sh
# Compile the binary to ./bin/herdr-launcher. herdr runs this as the [[build]]
# step on a GitHub install; also run by hand from a local checkout.
# (A published plugin would add a prebuilt-release-download fallback here.)
set -eu

root="$(cd "$(dirname "$0")" && pwd)"
cd "$root"

# Pick a cargo that actually runs (mise-managed rust is common and may not be
# globally activated, so its bare `cargo` shim fails — test before using).
if mise exec rust@stable -- cargo --version >/dev/null 2>&1; then
  CARGO="mise exec rust@stable -- cargo"
elif cargo --version >/dev/null 2>&1; then
  CARGO="cargo"
else
  echo "no working cargo/rust toolchain — install Rust (e.g. mise use -g rust)" >&2
  exit 1
fi

$CARGO build --release
mkdir -p bin
rm -f bin/herdr-launcher
cp target/release/herdr-launcher bin/herdr-launcher
# On Apple Silicon a cp'd Mach-O can fail the kernel code-signature check and get
# SIGKILLed (exit 137). Ad-hoc re-sign the copy to fix it — and fail loudly if that
# re-sign fails on macOS, rather than shipping a binary the kernel will kill. Skipped
# on non-Darwin, where codesign doesn't exist and isn't needed.
if [ "$(uname)" = "Darwin" ]; then
  codesign --force --sign - bin/herdr-launcher || {
    echo "codesign failed — the binary may be SIGKILLed (exit 137) on Apple Silicon" >&2
    exit 1
  }
fi
echo "built bin/herdr-launcher with: $CARGO"
