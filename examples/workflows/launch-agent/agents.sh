#!/usr/bin/env bash
set -uo pipefail

# Single parser for the agents roster (./agents, `name=command-prefix` lines; blank
# lines and `#` comments ignored). Shared by BOTH the picker (`agents.sh list`) and
# the launcher (`agents.sh get <name>`) so the two views can never drift.

bundle="$(cd "$(dirname "$0")" && pwd)"
file="$bundle/agents"

case "${1:-list}" in
  list)
    awk -F= '/^[[:space:]]*#/ || $0 !~ /=/ { next } { sub(/^[[:space:]]+/, "", $1); print $1 }' "$file"
    ;;
  get)
    awk -F= -v want="${2:-}" '
      /^[[:space:]]*#/ || $0 !~ /=/ { next }
      { name = $1; sub(/^[[:space:]]+/, "", name)
        if (name == want) { sub(/^[^=]*=/, ""); print; exit } }
    ' "$file"
    ;;
  *)
    echo "usage: agents.sh [list|get <name>]" >&2
    exit 2
    ;;
esac
