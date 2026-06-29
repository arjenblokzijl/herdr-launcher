# herdr-launcher

A **workflow launcher** for [Herdr](https://herdr.dev). Pick a workflow from a
menu inside herdr, fill in its fields, and it runs a command. The same workflows
work as a plain CLI (`herdr-launcher run <name> --field value …`), so they're
also scriptable.

herdr-plus's Quick Actions stop at a single input field; this fills the gap with
arbitrary fields per workflow.

## Requirements

- Herdr `>= 0.7.0`
- Node `>= 18` on `PATH`
- No runtime dependencies, no build step.

## Install

```bash
herdr plugin install arjenblokzijl/herdr-launcher
```

Optional — also expose it as a `herdr-launcher` CLI on your `PATH`. The plugin's
install path is shown by `herdr plugin list`; from that directory (or a local
checkout):

```bash
sh ./install.sh        # symlinks bin/herdr-launcher.mjs -> ~/.local/bin/herdr-launcher
```

In herdr, run the **"Launcher: pick & run"** action (or bind a key to it) to open
the picker. As a CLI: `herdr-launcher list`, `herdr-launcher run <name>`.

## Writing a workflow

A workflow is a `.mjs` file exporting a default object:

```js
export default {
  name: "greet",
  description: "Prints a greeting",
  fields: [
    { name: "name", prompt: "Your name", required: true },
    { name: "lang", prompt: "Language (en/nl)", default: "en" },
  ],
  // values = the collected field values; ctx = { cwd, env }
  async run(values, ctx) {
    console.log(`${values.lang === "nl" ? "Hallo" : "Hello"}, ${values.name}!`);
  },
};
```

Field keys: `name` (required), `prompt`, `required`, `default`. `fields` is
optional — a workflow with none just runs.

## Where workflows live

Loaded from the first match per name, in order:

1. `$HERDR_WORKFLOWS_DIR`
2. `$HERDR_PLUGIN_CONFIG_DIR/workflows` (this plugin's config dir, when run in herdr)
3. `~/.config/herdr-launcher/workflows/`
4. the plugin's bundled `examples/workflows/`

Put your own workflows in `~/.config/herdr-launcher/workflows/`. See
`examples/workflows/greet.mjs`.

## CLI

```bash
herdr-launcher list                          # list workflows
herdr-launcher new --title fix-login --prompt "implement login"   # shortcut for `run new-task`
herdr-launcher run <name>                    # prompts for missing fields (TTY)
herdr-launcher run <name> --help             # show a workflow's fields
```

A workflow's `run()` may call out to herdr (`$HERDR_BIN_PATH`) — those need a
running herdr server; pure workflows run anywhere.

## License

MIT
