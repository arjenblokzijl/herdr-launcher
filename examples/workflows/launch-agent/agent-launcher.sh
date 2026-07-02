#!/usr/bin/env bash
set -uo pipefail

# Orchestration for the "launch-agent" workflow. Reads field values from
# $HERDR_FIELD_*, creates a worktree off the chosen base branch, and starts the
# chosen agent in it via herdr's native `agent start`.
#
# Launch only — standalone, no dependency on any other plugin. It adds the agent
# in its OWN new pane and leaves the worktree's root pane untouched (never closes
# or reuses it). That is deliberate: any worktree.created hook (a setup tool, a
# layout manager, …) acts on the root pane, so leaving it alone lets this compose
# with such tools without the agent ever receiving their input. Per-worktree setup
# is NOT this tool's job — run it from a separate worktree.created plugin.
#
# The agent roster + launch commands live in ./agents (name=command-prefix), the
# single source of truth shared with the picker's choices_command.

bundle="${HERDR_WORKFLOW_DIR:-$(cd "$(dirname "$0")" && pwd)}"
flog="$HOME/.config/herdr/worktree-flow.log"
log() { echo "$(date '+%F %T') agent-launcher: $*" >> "$flog"; }

title="${HERDR_FIELD_title:-}"
prompt="${HERDR_FIELD_prompt:-}"
base="${HERDR_FIELD_base:-}"
agent="${HERDR_FIELD_agent:-}"
[ -n "$title" ] || { echo "title is required" >&2; exit 1; }
[ -n "$agent" ] || { echo "agent is required" >&2; exit 1; }
[ -n "$(printf '%s' "$prompt" | tr -d '[:space:]')" ] || { echo "prompt is required" >&2; exit 1; }

slug=$(printf '%s' "$title" | tr '[:upper:]' '[:lower:]' | sed -E 's/[^a-z0-9]+/-/g; s/^-+//; s/-+$//')
[ -n "$slug" ] || slug="task"

# Resolve the agent's launch command via the shared roster parser (agents.sh) — the
# single source of truth the picker also lists from.
agent_cmd="$(bash "$bundle/agents.sh" get "$agent")"
[ -n "$agent_cmd" ] || { echo "unknown agent '$agent' (not in $bundle/agents)" >&2; exit 1; }

dryrun=""
{ [ -e "$bundle/agent-launcher.DRYRUN" ] || [ -e "$HOME/.config/herdr/agent-launcher.DRYRUN" ]; } && dryrun=1

herdr="${HERDR_BIN_PATH:-herdr}"
repo="$PWD"
if [ -z "$dryrun" ]; then
  if ! git -C "$repo" rev-parse --git-dir >/dev/null 2>&1; then
    echo "not inside a git repo (cwd=$repo) — launch this from a workspace in your repo" >&2
    exit 1
  fi
  # Always create from the primary checkout. herdr errors when --cwd is itself a
  # linked worktree, so launching from inside a worktree would fail.
  main_repo="$(git -C "$repo" worktree list --porcelain | head -1 | cut -d' ' -f2-)"
  [ -n "$main_repo" ] && repo="$main_repo"

  # Repeated titles must not fail: bump -2, -3, … until the branch is free.
  base_slug="$slug"
  n=2
  while git -C "$repo" show-ref --verify --quiet "refs/heads/$slug"; do
    slug="${base_slug}-${n}"
    n=$((n + 1))
  done
fi

if [ -n "$dryrun" ]; then
  echo "[dry-run] slug=$slug  base=${base:-<current>}  agent=$agent"
  echo "[dry-run] worktree create --branch $slug ${base:+--base $base}"
  # shellcheck disable=SC2086
  echo "[dry-run] agent start $slug --cwd <new-worktree> --focus -- $agent_cmd \"$prompt\""
  exit 0
fi

log "create branch=$slug base=${base:-<current>} agent=$agent"

if [ -n "$base" ]; then
  resp=$("$herdr" worktree create --cwd "$repo" --branch "$slug" --base "$base" --focus --json 2>&1)
else
  resp=$("$herdr" worktree create --cwd "$repo" --branch "$slug" --focus --json 2>&1)
fi
wsid=$(printf '%s' "$resp" | jq -r '.result.workspace.workspace_id // empty')
path=$(printf '%s' "$resp" | jq -r '.result.worktree.path // empty')
if [ -z "$wsid" ]; then
  log "worktree create FAILED: $resp"
  echo "worktree create failed: $resp" >&2
  exit 1
fi
log "created path=$path wsid=$wsid; starting $agent"

# Start the agent in its OWN new pane; the worktree's root pane is left untouched
# for any setup hook to use. The name positional must be unique per agent (herdr
# rejects a duplicate), so use the worktree slug — not the roster key — or a second
# concurrent "claude" collides and fails to start. $agent_cmd is intentionally
# unquoted so its flags split into argv; the prompt is the final single argument.
# shellcheck disable=SC2086
exec "$herdr" agent start "$slug" --workspace "$wsid" --cwd "$path" --focus -- $agent_cmd "$prompt"
