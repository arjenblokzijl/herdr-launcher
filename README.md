# herdr-launcher

A **workflow launcher** for [Herdr](https://herdr.dev) with a [ratatui](https://ratatui.rs)
TUI. Pick a workflow from a menu inside herdr, fill in its fields — plain text,
multi-line, or a **fuzzy-searchable select** — and it runs a command. The same
workflows work as a plain CLI (`herdr-launcher run <name> --field value …`), so
they're also scriptable.

Workflows are **declarative TOML**; the whole thing ships as a single static Rust
binary with no runtime dependencies.

> **Beta (v0.2.x):** a from-scratch Rust rewrite of the earlier Node/`.mjs`
> version. The workflow format changed from `.mjs` to `.toml`.

## Requirements

- Herdr `>= 0.7.0`
- A Rust toolchain to build on install — `cargo` on `PATH`, or [mise](https://mise.jdx.dev)
  with `rust` (`mise use -g rust`). The `[[build]]` step compiles the binary.

## Install

```bash
herdr plugin install arjenblokzijl/herdr-launcher
```

In herdr, run the **"Launcher: pick & run"** action (or bind a key to it) to open
the picker. As a CLI: `herdr-launcher list`, `herdr-launcher run <name>`.

## Configuration

Launcher reads `config.toml` from its Herdr plugin config directory. Print that
directory with:

```bash
herdr plugin config-dir arjenblokzijl.herdr-launcher
```

When the binary runs outside Herdr, it falls back to
`~/.config/herdr-launcher/config.toml`. The file is optional:

```toml
# "split" (default) or "overlay"
placement = "overlay"

[workflows.greet]
profile = "personal"
```

An absent file defaults to a split pane. Invalid TOML or any placement other
than `split` or `overlay` opens a safe split pane containing the config path
and error; normal workflows cannot run from that diagnostic screen. Close and
reopen Launcher after changing the file.

Each `[workflows.<namespace>]` table contains string values exposed to matching
workflows as `$HERDR_WORKFLOW_CONFIG_<KEY>`. By default, the namespace is the
workflow's stable `name`; set `config_namespace` in a workflow when several
workflows should share one table. Config keys must be lowercase ASCII shell
identifiers and are uppercased in the environment.

## The picker

- **↑↓ / Enter** to choose a workflow, then a boxed field form.
- **Tab / Shift-Tab** move between fields; **Ctrl+S** submits; **Esc** backs out.
- Fields render by type: single-line, multi-line (Enter inserts a newline), and a
  fuzzy **select** (type to filter, ↑↓ to pick) for `choices_command` fields.

## Writing a workflow

A workflow is a `.toml` file — either `name.toml` directly, or a self-contained
`name/` **bundle directory** (its `.toml` plus any helper scripts it calls):

```toml
name = "greet"
title = "Say Hello" # optional display label; `name` remains the CLI identifier
# config_namespace = "greetings" # optional shared [workflows.greetings] config
description = "Prints a greeting"
# Optional: exit zero when runnable; explain failures on stderr.
availability_command = "test -n \"$HERDR_WORKFLOW_CONFIG_PROFILE\" || { echo 'Set a profile in Launcher config' >&2; exit 1; }"
# Field values are passed via $HERDR_FIELD_<name> (never interpolated into the
# shell — no injection). $HERDR_WORKFLOW_DIR points at this workflow's folder.
command = "echo \"Hi $HERDR_FIELD_name — lang=$HERDR_FIELD_lang\""

[[fields]]
name = "name"
prompt = "Your name"
required = true

[[fields]]
name = "lang"
prompt = "Language (en/nl)"
default = "en"
```

`name` is the stable identifier used by `run` and configuration. An optional,
non-empty `title` is shown in the picker. `list` includes the stable name in
brackets when it differs from the title, so its output remains directly runnable.

Field keys:

| key | meaning |
|-----|---------|
| `name` | required; the `$HERDR_FIELD_<name>` key |
| `prompt` | label shown in the form |
| `required` | block submit until non-empty |
| `default` | initial value / preselected choice |
| `multiline` | render a multi-line text area |
| `choices_command` | shell command whose stdout lines become a fuzzy-select list |
| `depends_on` | earlier field names whose resolved values affect this choice list |

`fields` is optional — a workflow with none just runs its `command`.

A `choices_command` with `depends_on = ["project"]` receives only that
dependency as `$HERDR_FIELD_project`, plus the normal workflow context.
Dependencies must name earlier fields. Launcher refreshes a dependent list when
its inputs change, including immediately before submission.

Choice commands run synchronously, so Launcher displays `Loading choices…`
before evaluating them. Successful empty output remains an empty, constrained
select. A nonzero exit is shown as an error and retried when the field is entered
again or the form is submitted. Neither case falls back to free text in the TUI
or CLI. Output values preserve leading and trailing spaces.

An optional `availability_command` runs when workflows load. It receives
`$HERDR_WORKFLOW_DIR` and the resolved `$HERDR_WORKFLOW_CONFIG_*` values, but no
field values. Exit zero enables the workflow. A nonzero exit keeps it visible as
disabled, displays its sanitized stderr as the reason, and prevents both TUI and
CLI execution. Keep the check fast because workflow loading is synchronous.

### Bundles + `$HERDR_WORKFLOW_DIR`

A `name/` directory is loaded as one workflow, and `$HERDR_WORKFLOW_DIR` is set to
that directory for both `command` and `choices_command`. This lets a workflow
reference its own bundled scripts relocatably and keeps everything for a task in
one folder (delete the folder = delete the task):

```
myflow/
  myflow.toml        # choices_command = "bash $HERDR_WORKFLOW_DIR/list.sh"
  list.sh            # command         = "bash $HERDR_WORKFLOW_DIR/run.sh"
  run.sh
```

## Where workflows live

Loaded from the first match per name, in order:

1. `$HERDR_WORKFLOWS_DIR`
2. `$HERDR_PLUGIN_CONFIG_DIR/workflows` (this plugin's config dir, in herdr)
3. `~/.config/herdr-launcher/workflows/`
4. the plugin's bundled `examples/workflows/`

Put your own workflows in `~/.config/herdr-launcher/workflows/`. See
`examples/workflows/`.

The bundled **`launch-agent`** example is a real bundle: it creates a git worktree
off a chosen base branch and starts a coding agent in it via herdr's native
`agent start`, reading its agent roster from an `agents` file (`name=command`).
It is launch-only by design — it adds the agent in its own pane and leaves the
worktree's root pane untouched, so a separate `worktree.created` hook (deps setup,
a layout manager, …) can act on that pane without ever colliding with the agent.

## CLI

```bash
herdr-launcher list                 # list workflows
herdr-launcher pick                 # interactive picker (needs a TTY)
herdr-launcher run <name>           # prompts for missing fields (TTY); --field value to preset
```

A workflow's `command` may call out to herdr (`$HERDR_BIN_PATH`) — those need a
running herdr server; pure commands run anywhere.

## License

MIT
