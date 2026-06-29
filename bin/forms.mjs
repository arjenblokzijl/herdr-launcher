#!/usr/bin/env node
// Multi-field form runner. Usable as a CLI and as a herdr plugin.
//   forms list                              list available forms
//   forms run <name> [--field value ...]    run a form (prompts for missing fields when a TTY)
//   forms pick                              interactive picker (used by the herdr pane)
//   forms open                              open the picker as a herdr pane (used by the action)
// Forms are config-as-code .mjs files exporting { name, description, fields[], run() }.
// Loaded from $HERDR_FORMS_DIR, $HERDR_PLUGIN_CONFIG_DIR/forms, ~/.config/herdr/forms,
// and the plugin's bundled examples/ (earlier dirs win on name clash).

import { readdirSync, existsSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";
import { homedir } from "node:os";
import { createInterface } from "node:readline";
import { execFileSync } from "node:child_process";

const here = dirname(fileURLToPath(import.meta.url));

function formDirs() {
  const dirs = [];
  if (process.env.HERDR_FORMS_DIR) dirs.push(process.env.HERDR_FORMS_DIR);
  if (process.env.HERDR_PLUGIN_CONFIG_DIR) dirs.push(join(process.env.HERDR_PLUGIN_CONFIG_DIR, "forms"));
  dirs.push(join(homedir(), ".config", "herdr", "forms"));
  dirs.push(join(here, "..", "examples", "forms"));
  return dirs;
}

async function loadForms() {
  const byName = new Map();
  for (const dir of formDirs()) {
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

async function collect(form, flags) {
  const interactive = process.stdin.isTTY && !flags["no-input"];
  const values = {};
  for (const field of form.fields || []) {
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
  const cmd = positionals[0] || "list";
  const forms = await loadForms();

  if (cmd === "list") {
    console.log("Available forms:");
    for (const f of forms) console.log(`  ${f.name}\t${f.description || ""}`);
    return;
  }

  if (cmd === "open") {
    const herdr = process.env.HERDR_BIN_PATH || "herdr";
    const pluginId = process.env.HERDR_PLUGIN_ID;
    if (!pluginId) throw new Error("HERDR_PLUGIN_ID not set — run via the herdr action");
    execFileSync(herdr, ["plugin", "pane", "open", "--plugin", pluginId, "--entrypoint", "forms-ui", "--placement", "overlay"], { stdio: "inherit" });
    return;
  }

  if (cmd === "pick") {
    if (!forms.length) {
      console.log("No forms found. Add .mjs files to ~/.config/herdr/forms/");
      await ask("Press Enter to close…");
      return;
    }
    console.log("Forms:\n");
    forms.forEach((f, i) => console.log(`  ${i + 1}) ${f.name}  —  ${f.description || ""}`));
    console.log("");
    const sel = (await ask("Pick a form (number or name): ")).trim();
    const form = forms[parseInt(sel, 10) - 1] || forms.find((f) => f.name === sel);
    if (!form) {
      console.error(`no such form: ${sel}`);
      await ask("Press Enter to close…");
      return;
    }
    try {
      await form.run(await collect(form, {}), ctx());
    } catch (e) {
      console.error(String(e?.message || e));
      await ask("Press Enter to close…");
    }
    return;
  }

  if (cmd === "run") {
    const form = forms.find((f) => f.name === positionals[1]);
    if (!form) throw new Error(`unknown form: ${positionals[1] || "(none given)"}`);
    if (flags.help) {
      console.log(`${form.name} — ${form.description || ""}\nFields:`);
      for (const fl of form.fields || [])
        console.log(`  --${fl.name}\t${fl.prompt || ""}${fl.required ? " (required)" : ""}`);
      return;
    }
    await form.run(await collect(form, flags), ctx());
    return;
  }

  throw new Error(`unknown command: ${cmd} (use: list | run <name> | pick | open)`);
}

main().catch((e) => {
  console.error(String(e?.message || e));
  process.exit(1);
});
