#!/usr/bin/env sh
# Symlink the runner onto your PATH so `herdr-launcher` works as a CLI.
# Run it by hand from a local checkout (herdr can also run it as a [[build]] step).
set -eu

root="$(cd "$(dirname "$0")" && pwd)"
chmod +x "$root/bin/herdr-launcher.mjs"

bindir="${HOME}/.local/bin"
mkdir -p "$bindir"
ln -sfn "$root/bin/herdr-launcher.mjs" "$bindir/herdr-launcher"

echo "Linked $bindir/herdr-launcher -> $root/bin/herdr-launcher.mjs"
echo "Put your forms in ~/.config/herdr/forms/ (see examples/forms/)."
