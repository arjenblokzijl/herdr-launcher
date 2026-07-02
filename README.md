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
description = "Prints a greeting"
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

Field keys:

| key | meaning |
|-----|---------|
| `name` | required; the `$HERDR_FIELD_<name>` key |
| `prompt` | label shown in the form |
| `required` | block submit until non-empty |
| `default` | initial value / preselected choice |
| `multiline` | render a multi-line text area |
| `choices_command` | shell command whose stdout lines become a fuzzy-select list |

`fields` is optional — a workflow with none just runs its `command`.

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
