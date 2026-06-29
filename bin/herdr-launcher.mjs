#!/usr/bin/env node
// herdr-launcher — a workflow launcher. Usable as a CLI and as a herdr plugin.
//   herdr-launcher list                          list available workflows
//   herdr-launcher new [--field value ...]       shortcut for `run new-task`
//   herdr-launcher run <name> [--field value ...] run a workflow (prompts for missing fields when a TTY)
//   herdr-launcher pick                          interactive picker (used by the herdr pane)
//   herdr-launcher open                          open the picker as a herdr pane (used by the action)
// Workflows are config-as-code .mjs files exporting { name, description, fields[], run() }.
// Loaded from $HERDR_WORKFLOWS_DIR, $HERDR_PLUGIN_CONFIG_DIR/workflows,
// ~/.config/herdr-launcher/workflows, and the plugin's bundled examples/ (earlier dirs win).

import { readdirSync, existsSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";
import { homedir } from "node:os";
import { createInterface } from "node:readline";
import { execFileSync } from "node:child_process";

const here = dirname(fileURLToPath(import.meta.url));

function workflowDirs() {
  const dirs = [];
  if (process.env.HERDR_WORKFLOWS_DIR) dirs.push(process.env.HERDR_WORKFLOWS_DIR);
  if (process.env.HERDR_PLUGIN_CONFIG_DIR) dirs.push(join(process.env.HERDR_PLUGIN_CONFIG_DIR, "workflows"));
  dirs.push(join(homedir(), ".config", "herdr-launcher", "workflows"));
  dirs.push(join(here, "..", "examples", "workflows"));
  return dirs;
}

async function loadWorkflows() {
  const byName = new Map();
  for (const dir of workflowDirs()) {
    if (!existsSync(dir)) continue;
    for (const file of readdirSync(dir).filter((f) => f.endsWith(".mjs"))) {
      const mod = await import(pathToFileURL(join(dir, file)).href);
      if (mod.default?.name && !byName.has(mod.default.name)) byName.set(mod.default.name, mod.default);
    }
  }
  return [...byName.values()].sort((a, b) => a.name.localeCompare(b.name));
}

function parseArgs(argv) {
  const flags = {};
  const positionals = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith("--")) {
      positionals.push(a);
      continue;
    }
    const key = a.slice(2);
    if (key.includes("=")) {
      const [k, ...v] = key.split("=");
      flags[k] = v.join("=");
    } else if (i + 1 < argv.length && !argv[i + 1].startsWith("--")) {
      flags[key] = argv[++i];
    } else {
      flags[key] = true;
    }
  }
  return { positionals, flags };
}

function ask(question) {
  const rl = createInterface({ input: process.stdin, output: process.stdout });
  return new Promise((res) => rl.question(question, (a) => (rl.close(), res(a))));
}

async function collect(workflow, flags) {
  const interactive = process.stdin.isTTY && !flags["no-input"];
  const values = {};
  for (const field of workflow.fields || []) {
    let val = flags[field.name];
    if (val === undefined || val === true) {
      if (interactive) {
        const def = field.default !== undefined ? ` [${field.default}]` : "";
        val = (await ask(`${field.prompt || field.name}${def}: `)).trim();
        while (field.required && !val && field.default === undefined) {
          val = (await ask(`${field.prompt || field.name} (required): `)).trim();
        }
        if (!val && field.default !== undefined) val = field.default;
      } else if (field.default !== undefined) {
        val = field.default;
      } else if (field.required) {
        throw new Error(`missing required field --${field.name} (no TTY for prompt)`);
      } else {
        val = "";
      }
    }
    values[field.name] = val;
  }
  return values;
}

const ctx = () => ({ cwd: process.cwd(), env: process.env });

async function main() {
  const { positionals, flags } = parseArgs(process.argv.slice(2));
  let cmd = positionals[0] || "list";
  // `new` is a shortcut for `run new-task`
  if (cmd === "new") {
    positionals.splice(0, 1, "run", "new-task");
    cmd = "run";
  }
  const workflows = await loadWorkflows();

  if (cmd === "list") {
    console.log("Available workflows:");
    for (const w of workflows) console.log(`  ${w.name}\t${w.description || ""}`);
    return;
  }

  if (cmd === "open") {
    const herdr = process.env.HERDR_BIN_PATH || "herdr";
    const pluginId = process.env.HERDR_PLUGIN_ID;
    if (!pluginId) throw new Error("HERDR_PLUGIN_ID not set — run via the herdr action");
    execFileSync(herdr, ["plugin", "pane", "open", "--plugin", pluginId, "--entrypoint", "launcher-ui", "--placement", "overlay"], { stdio: "inherit" });
    return;
  }

  if (cmd === "pick") {
    if (!workflows.length) {
      console.log("No workflows found. Add .mjs files to ~/.config/herdr-launcher/workflows/");
      await ask("Press Enter to close…");
      return;
    }
    console.log("Workflows:\n");
    workflows.forEach((w, i) => console.log(`  ${i + 1}) ${w.name}  —  ${w.description || ""}`));
    console.log("");
    const sel = (await ask("Pick a workflow (number or name): ")).trim();
    const workflow = workflows[parseInt(sel, 10) - 1] || workflows.find((w) => w.name === sel);
    if (!workflow) {
      console.error(`no such workflow: ${sel}`);
      await ask("Press Enter to close…");
      return;
    }
    try {
      await workflow.run(await collect(workflow, {}), ctx());
    } catch (e) {
      console.error(String(e?.message || e));
      await ask("Press Enter to close…");
    }
    return;
  }

  if (cmd === "run") {
    const workflow = workflows.find((w) => w.name === positionals[1]);
    if (!workflow) throw new Error(`unknown workflow: ${positionals[1] || "(none given)"}`);
    if (flags.help) {
      console.log(`${workflow.name} — ${workflow.description || ""}\nFields:`);
      for (const fl of workflow.fields || [])
        console.log(`  --${fl.name}\t${fl.prompt || ""}${fl.required ? " (required)" : ""}`);
      return;
    }
    await workflow.run(await collect(workflow, flags), ctx());
    return;
  }

  throw new Error(`unknown command: ${cmd} (use: list | new | run <name> | pick | open)`);
}

main().catch((e) => {
  console.error(String(e?.message || e));
  process.exit(1);
});
