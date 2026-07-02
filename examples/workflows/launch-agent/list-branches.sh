#!/usr/bin/env bash
set -uo pipefail

# Candidate list for the "Starting branch" picker (a choices_command field).
# Prints one ref per line: the repo default branch first, then the other local
# branches, then remote-only branches (a remote is hidden when a local of the same
# short name already exists). The launcher fuzzy-filters this list.

repo="$PWD"
git -C "$repo" rev-parse --git-dir >/dev/null 2>&1 || exit 0

# Default branch: origin/HEAD, else the current branch, else a common name that
# actually exists (detached HEAD / unset origin/HEAD would otherwise drop it).
default="$(git -C "$repo" symbolic-ref --short refs/remotes/origin/HEAD 2>/dev/null | sed 's@^origin/@@')"
[ -n "$default" ] || default="$(git -C "$repo" branch --show-current 2>/dev/null)"
if [ -z "$default" ]; then
  for cand in main master; do
    if git -C "$repo" show-ref --verify --quiet "refs/heads/$cand"; then default="$cand"; break; fi
  done
fi

locals="$(git -C "$repo" for-each-ref --format='%(refname:short)' refs/heads)"

remote_only() {
  # Emit each remote ref whose short name isn't already a local branch. Fixed-string,
  # whole-line matching (grep -qxF) — no glob/pipe metacharacter hazards.
  git -C "$repo" for-each-ref --format='%(refname:short)' refs/remotes | while IFS= read -r ref; do
    case "$ref" in */HEAD) continue ;; esac
    printf '%s\n' "$locals" | grep -qxF "${ref#*/}" || printf '%s\n' "$ref"
  done
}

# Order: default, locals, remote-only — then awk drops blanks and exact-line dupes
# (idiomatic set-membership dedup; the default reappearing among locals collapses).
{
  [ -n "$default" ] && printf '%s\n' "$default"
  printf '%s\n' "$locals"
  remote_only
} | awk 'NF && !seen[$0]++'
