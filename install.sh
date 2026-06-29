#!/usr/bin/env sh
# Symlink the runner onto your PATH so `forms` works as a CLI.
# herdr runs this as the plugin's [[build]] step on install; you can also run it
# by hand from a local checkout.
set -eu

root="$(cd "$(dirname "$0")" && pwd)"
chmod +x "$root/bin/forms.mjs"

bindir="${HOME}/.local/bin"
mkdir -p "$bindir"
ln -sfn "$root/bin/forms.mjs" "$bindir/forms"

echo "Linked $bindir/forms -> $root/bin/forms.mjs"
echo "Put your forms in ~/.config/herdr/forms/ (see examples/forms/)."
