// herdr-launcher (Rust) — a workflow launcher with a ratatui TUI.
// Workflows are declarative TOML (fields + a command that reads $HERDR_FIELD_<name>).
//   herdr-launcher list
//   herdr-launcher run <name> [--field value ...]
//   herdr-launcher pick            (ratatui picker + field form; falls back to stdin without a tty)
//   herdr-launcher __snapshot      (render the TUI to text via TestBackend — for demos/tests)
// Workflows: *.toml in $HERDR_WORKFLOWS_DIR and ~/.config/herdr-launcher/workflows/.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;
use serde::Deserialize;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;
use tui_textarea::{CursorMove, TextArea};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};

#[derive(Deserialize, Clone)]
struct Field {
    name: String,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    multiline: bool,
    #[serde(default)]
    choices_command: Option<String>,
}

#[derive(Deserialize, Clone)]
struct Workflow {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    fields: Vec<Field>,
    command: String,
    #[serde(skip)]
    dir: PathBuf,
}

fn workflow_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(d) = env::var("HERDR_WORKFLOWS_DIR") {
        dirs.push(PathBuf::from(d));
    }
    if let Ok(d) = env::var("HERDR_PLUGIN_CONFIG_DIR") {
        dirs.push(PathBuf::from(d).join("workflows"));
    }
    if let Some(home) = env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".config/herdr-launcher/workflows"));
    }
    if let Ok(d) = env::var("HERDR_PLUGIN_ROOT") {
        dirs.push(PathBuf::from(d).join("examples/workflows"));
    }
    dirs
}

fn is_toml(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("toml")
}

// Directory iteration order is unspecified, so sort by path before loading. This
// makes "first workflow with a name wins" deterministic instead of dependent on the
// filesystem's whims.
fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    paths
}

// Field values are exported as $HERDR_FIELD_<name>, so a name must be a valid POSIX
// shell identifier — otherwise the value is set but unreferenceable (e.g. a field named
// "branch-name" produces HERDR_FIELD_branch-name, which sh cannot expand).
fn valid_field_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

// A workflow's `dir` is where its .toml lives, exposed to the command and
// choices_command as $HERDR_WORKFLOW_DIR so a workflow can reference its own
// bundled helper scripts relocatably.
fn load_toml_file(path: &Path, dir: &Path, out: &mut Vec<Workflow>) {
    let Ok(text) = fs::read_to_string(path) else { return };
    match toml::from_str::<Workflow>(&text) {
        Ok(mut w) if !out.iter().any(|x| x.name == w.name) => {
            if let Some(bad) = w.fields.iter().find(|f| !valid_field_name(&f.name)) {
                eprintln!(
                    "skip {}: field name '{}' is not a valid shell identifier ([A-Za-z_][A-Za-z0-9_]*)",
                    path.display(),
                    bad.name
                );
                return;
            }
            w.dir = dir.to_path_buf();
            out.push(w);
        }
        Ok(_) => {}
        Err(e) => eprintln!("skip {}: {}", path.display(), e),
    }
}

fn load_bundle(dir: &Path, out: &mut Vec<Workflow>) {
    for path in sorted_entries(dir) {
        if is_toml(&path) {
            load_toml_file(&path, dir, out);
        }
    }
}

// A workflow is either a flat <name>.toml or a self-contained <name>/ bundle dir
// (its .toml plus any helper scripts it references). Bundles are scanned one level
// deep — deep enough for "one folder per task", shallow enough to stay predictable.
fn load_dir(dir: &Path, out: &mut Vec<Workflow>) {
    for path in sorted_entries(dir) {
        if is_toml(&path) {
            load_toml_file(&path, dir, out);
        } else if path.is_dir() {
            load_bundle(&path, out);
        }
    }
}

fn load_workflows() -> Vec<Workflow> {
    let mut out: Vec<Workflow> = Vec::new();
    for dir in workflow_dirs() {
        load_dir(&dir, &mut out);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn run_workflow(wf: &Workflow, values: &HashMap<String, String>) -> ! {
    // The command reads field values from $HERDR_FIELD_<name> (and $HERDR_WORKFLOW_DIR
    // for bundled scripts). Values are passed via the environment, never interpolated
    // into the command string, so a field containing shell metacharacters can't inject.
    let mut command = Command::new("sh");
    command.arg("-c").arg(&wf.command);
    command.env("HERDR_WORKFLOW_DIR", &wf.dir);
    for (k, v) in values {
        command.env(format!("HERDR_FIELD_{k}"), v);
    }
    match command.status() {
        Ok(s) => std::process::exit(s.code().unwrap_or(0)),
        Err(e) => {
            eprintln!("failed to run: {e}");
            std::process::exit(1);
        }
    }
}

// ---------- TUI ----------

enum Screen {
    List,
    Form,
}

enum FieldInput {
    Line(Input),
    Area(Box<TextArea<'static>>),
    Select(Box<SelectField>),
}

impl FieldInput {
    fn value(&self) -> String {
        match self {
            FieldInput::Line(input) => input.value().to_string(),
            FieldInput::Area(area) => area.lines().join("\n"),
            FieldInput::Select(select) => select.value.clone(),
        }
    }

    fn handle(&mut self, key: KeyEvent) {
        match self {
            FieldInput::Line(input) => {
                input.handle_event(&Event::Key(key));
            }
            FieldInput::Area(area) => {
                area.input(key);
            }
            FieldInput::Select(select) => select.handle(key),
        }
    }
}

// A fuzzy-searchable single-choice field. Matching is synchronous (nucleo-matcher's
// one-shot match_list) — the candidate set is a small finite list (e.g. git branches),
// so no async driver / worker threads are needed.
struct SelectField {
    choices: Vec<String>,
    query: Input,
    filtered: Vec<String>,
    sel: usize,
    value: String,
    matcher: Matcher,
}

impl SelectField {
    fn new(choices: Vec<String>, default: Option<String>) -> Self {
        let mut select = SelectField {
            choices,
            query: Input::default(),
            filtered: vec![],
            sel: 0,
            value: String::new(),
            matcher: Matcher::new(Config::DEFAULT),
        };
        select.refilter();
        if let Some(d) = default {
            if let Some(pos) = select.filtered.iter().position(|c| *c == d) {
                select.sel = pos;
            }
        }
        select.commit();
        select
    }

    fn refilter(&mut self) {
        let query = self.query.value().to_string();
        if query.is_empty() {
            self.filtered = self.choices.clone();
        } else {
            let pattern = Pattern::parse(&query, CaseMatching::Smart, Normalization::Smart);
            self.filtered = pattern
                .match_list(self.choices.iter().cloned(), &mut self.matcher)
                .into_iter()
                .map(|(choice, _)| choice)
                .collect();
        }
        if self.sel >= self.filtered.len() {
            self.sel = self.filtered.len().saturating_sub(1);
        }
    }

    fn commit(&mut self) {
        // Clear the value when nothing matches, so a zero-match query can't submit a
        // stale (previously-highlighted) choice.
        self.value = self.filtered.get(self.sel).cloned().unwrap_or_default();
    }

    fn handle(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => {
                if self.sel + 1 < self.filtered.len() {
                    self.sel += 1;
                }
            }
            _ => {
                self.query.handle_event(&Event::Key(key));
                self.refilter();
                self.sel = 0;
            }
        }
        self.commit();
    }
}

fn run_choices(command: &str, workflow_dir: &Path) -> Vec<String> {
    let Ok(output) = Command::new("sh")
        .arg("-c")
        .arg(command)
        .env("HERDR_WORKFLOW_DIR", workflow_dir)
        .output()
    else {
        return vec![];
    };
    if !output.status.success() {
        return vec![];
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

fn build_input(field: &Field, workflow_dir: &Path) -> FieldInput {
    let default = field.default.clone().unwrap_or_default();
    if let Some(command) = &field.choices_command {
        // Always a Select (even with no candidates) so a choices_command field stays
        // constrained — never silently downgraded to unvalidated free text.
        let choices = run_choices(command, workflow_dir);
        return FieldInput::Select(Box::new(SelectField::new(choices, field.default.clone())));
    }
    if field.multiline {
        let mut area = if default.is_empty() {
            TextArea::default()
        } else {
            TextArea::new(default.split('\n').map(str::to_string).collect())
        };
        area.move_cursor(CursorMove::Bottom);
        area.move_cursor(CursorMove::End);
        FieldInput::Area(Box::new(area))
    } else {
        FieldInput::Line(Input::new(default))
    }
}

struct App {
    workflows: Vec<Workflow>,
    selected: usize,
    screen: Screen,
    form_idx: usize,           // index into workflows of the workflow being filled
    inputs: Vec<FieldInput>,   // current editor per field
    field_idx: usize,
    has_form: bool,            // whether `inputs` holds a form for `form_idx`
    error: Option<String>,
    submit: Option<(Workflow, HashMap<String, String>)>,
}

impl App {
    fn new(workflows: Vec<Workflow>) -> Self {
        App { workflows, selected: 0, screen: Screen::List, form_idx: 0, inputs: vec![], field_idx: 0, has_form: false, error: None, submit: None }
    }

    fn move_list(&mut self, delta: isize) {
        let n = self.workflows.len() as isize;
        if n == 0 {
            return;
        }
        self.selected = (((self.selected as isize) + delta).rem_euclid(n)) as usize;
    }

    fn open_form(&mut self) {
        // Rebuild only when entering a different workflow. Re-opening the same one
        // (e.g. after Esc to peek at the list) keeps whatever was already typed.
        if !self.has_form || self.form_idx != self.selected {
            self.form_idx = self.selected;
            let wf = &self.workflows[self.form_idx];
            let dir = wf.dir.clone();
            self.inputs = wf.fields.iter().map(|field| build_input(field, &dir)).collect();
            self.field_idx = 0;
            self.has_form = true;
        }
        self.error = None;
        self.screen = Screen::Form;
    }

    fn move_field(&mut self, delta: isize) {
        let n = self.workflows[self.form_idx].fields.len() as isize;
        if n == 0 {
            return;
        }
        self.field_idx = (((self.field_idx as isize) + delta).rem_euclid(n)) as usize;
    }

    fn current_multiline(&self) -> bool {
        matches!(self.inputs.get(self.field_idx), Some(FieldInput::Area(_)))
    }

    fn try_submit(&mut self) {
        let wf = self.workflows[self.form_idx].clone();
        let mut values: Vec<String> = self.inputs.iter().map(FieldInput::value).collect();
        for (i, f) in wf.fields.iter().enumerate() {
            if values[i].trim().is_empty() {
                if let Some(d) = &f.default {
                    values[i] = d.clone();
                }
            }
        }
        for (i, f) in wf.fields.iter().enumerate() {
            if f.required && values[i].trim().is_empty() {
                self.field_idx = i;
                let label = f.prompt.clone().unwrap_or_else(|| f.name.clone());
                self.error = Some(format!("'{label}' is required"));
                return;
            }
        }
        let map = wf
            .fields
            .iter()
            .enumerate()
            .map(|(i, f)| (f.name.clone(), values[i].clone()))
            .collect();
        self.submit = Some((wf, map));
    }
}

// The half-open range of field indices to render: always contains `focus`, grown
// outward (following fields first, then preceding) until the next field wouldn't fit
// in `avail` rows. A field taller than the whole pane is still returned alone so it at
// least renders (clipped) rather than vanishing.
fn visible_window(heights: &[u16], focus: usize, avail: u16) -> (usize, usize) {
    if heights.is_empty() {
        return (0, 0);
    }
    let focus = focus.min(heights.len() - 1);
    let mut start = focus;
    let mut end = focus + 1;
    let mut used = heights[focus];
    loop {
        let mut grew = false;
        if end < heights.len() && used + heights[end] <= avail {
            used += heights[end];
            end += 1;
            grew = true;
        }
        if start > 0 && used + heights[start - 1] <= avail {
            used += heights[start - 1];
            start -= 1;
            grew = true;
        }
        if !grew {
            break;
        }
    }
    (start, end)
}

fn ui(f: &mut Frame, app: &mut App) {
    f.render_widget(Clear, f.area());
    let area = f.area();
    match app.screen {
        Screen::List => {
            // No inner border — herdr already draws the pane frame ("Launcher").
            let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
            let items: Vec<ListItem> = app
                .workflows
                .iter()
                .map(|w| ListItem::new(format!(" {}  —  {}", w.name, w.description)))
                .collect();
            let list = List::new(items)
                .highlight_symbol("▶ ")
                .highlight_style(Style::new().bold().fg(Color::Cyan));
            let mut state = ListState::default();
            state.select(Some(app.selected));
            f.render_stateful_widget(list, rows[0], &mut state);
            f.render_widget(
                Paragraph::new(Line::from(" ↑↓ move · Enter select · Esc close").dim()),
                rows[1],
            );
        }
        Screen::Form => {
            let App { workflows, form_idx, field_idx, inputs, error, .. } = app;
            let wf = &workflows[*form_idx];

            let heights: Vec<u16> = inputs
                .iter()
                .enumerate()
                .map(|(i, input)| match input {
                    FieldInput::Area(_) => 8,
                    FieldInput::Select(_) if i == *field_idx => 10,
                    _ => 3,
                })
                .collect();

            // The pane may be too short to show every field, so render a window that
            // always contains the focused field (grown outward until it overflows).
            // Tab therefore never parks the cursor on a field that isn't on screen.
            let avail = area.height.saturating_sub(4); // header (3) + footer (1)
            let (start, end) = visible_window(&heights, *field_idx, avail);

            let mut constraints = vec![Constraint::Length(3)];
            for &h in &heights[start..end] {
                constraints.push(Constraint::Length(h));
            }
            constraints.push(Constraint::Min(0));
            constraints.push(Constraint::Length(1));
            let rows = Layout::vertical(constraints).split(area);

            f.render_widget(
                Paragraph::new(vec![
                    Line::from(format!(" {}", wf.name)).bold(),
                    Line::from(format!(" {}", wf.description)).dim(),
                ]),
                rows[0],
            );

            for (slot, i) in (start..end).enumerate() {
                let field = &wf.fields[i];
                let label = field.prompt.clone().unwrap_or_else(|| field.name.clone());
                let req = if field.required { " *" } else { "" };
                let focused = i == *field_idx;
                let rect = rows[slot + 1];
                let title = format!(" {label}{req} ");
                let block = Block::bordered()
                    .border_type(if focused { BorderType::Thick } else { BorderType::Plain })
                    .border_style(if focused { Style::new().cyan() } else { Style::new().dim() })
                    .title(if focused { Span::from(title).bold().cyan() } else { Span::from(title).dim() });

                match &mut inputs[i] {
                    FieldInput::Area(area) => {
                        if focused {
                            area.set_cursor_style(Style::new().reversed());
                        } else {
                            area.set_cursor_style(Style::default());
                        }
                        area.set_cursor_line_style(Style::default());
                        area.set_block(block);
                        f.render_widget(&**area, rect);
                    }
                    FieldInput::Line(input) => {
                        let inner_w = rect.width.max(3) - 3;
                        let scroll = input.visual_scroll(inner_w as usize);
                        f.render_widget(
                            Paragraph::new(input.value()).scroll((0, scroll as u16)).block(block),
                            rect,
                        );
                        if focused {
                            let cx = rect.x + 1 + (input.visual_cursor().max(scroll) - scroll) as u16;
                            f.set_cursor_position((cx, rect.y + 1));
                        }
                    }
                    FieldInput::Select(select) => {
                        if !focused {
                            f.render_widget(Paragraph::new(select.value.as_str()).block(block), rect);
                            continue;
                        }
                        let block = block.title_bottom(
                            Line::from(format!(" {} matches ", select.filtered.len())).dim().right_aligned(),
                        );
                        let inner = block.inner(rect);
                        f.render_widget(block, rect);
                        let [search, list] =
                            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);

                        let prefix = "› ";
                        f.render_widget(
                            Paragraph::new(format!("{prefix}{}", select.query.value())),
                            search,
                        );
                        let cx = search.x + prefix.chars().count() as u16 + select.query.visual_cursor() as u16;
                        f.set_cursor_position((cx, search.y));

                        let items: Vec<ListItem> =
                            select.filtered.iter().map(|c| ListItem::new(c.as_str())).collect();
                        let mut state = ListState::default();
                        if !select.filtered.is_empty() {
                            state.select(Some(select.sel));
                        }
                        f.render_stateful_widget(
                            List::new(items)
                                .highlight_symbol("▶ ")
                                .highlight_style(Style::new().reversed()),
                            list,
                            &mut state,
                        );
                    }
                }
            }

            let last = rows.len() - 1;
            let footer = if let Some(err) = error.as_deref() {
                Line::from(format!(" {err}")).style(Style::new().red().bold())
            } else {
                let hint = if matches!(inputs.get(*field_idx), Some(FieldInput::Select(_))) {
                    " Type to filter · ↑↓ select · Enter next · Ctrl+S submit · Esc back"
                } else {
                    " Tab move · Enter next/newline · Ctrl+S submit · Esc back"
                };
                let more = if start > 0 || end < inputs.len() {
                    format!("  ·  field {}/{}", *field_idx + 1, inputs.len())
                } else {
                    String::new()
                };
                Line::from(format!("{hint}{more}")).dim()
            };
            f.render_widget(Paragraph::new(footer), rows[last]);
        }
    }
}

fn run_tui(workflows: Vec<Workflow>) -> Option<(Workflow, HashMap<String, String>)> {
    let mut terminal = ratatui::init();
    let _ = terminal.clear();
    let mut app = App::new(workflows);
    loop {
        let _ = terminal.draw(|f| ui(f, &mut app));
        let Ok(Event::Key(key)) = event::read() else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match app.screen {
            Screen::List => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Down | KeyCode::Char('j') => app.move_list(1),
                KeyCode::Up | KeyCode::Char('k') => app.move_list(-1),
                KeyCode::Enter => app.open_form(),
                _ => {}
            },
            Screen::Form => {
                app.error = None;
                let multiline = app.current_multiline();
                match key.code {
                    KeyCode::Esc => app.screen = Screen::List,
                    KeyCode::Tab => app.move_field(1),
                    KeyCode::BackTab => app.move_field(-1),
                    KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.try_submit()
                    }
                    KeyCode::Enter if !multiline => {
                        let last = app.field_idx + 1 >= app.workflows[app.form_idx].fields.len();
                        if last {
                            app.try_submit();
                        } else {
                            app.move_field(1);
                        }
                    }
                    _ if !app.inputs.is_empty() => app.inputs[app.field_idx].handle(key),
                    _ => {}
                }
            }
        }
        if app.submit.is_some() {
            break;
        }
    }
    ratatui::restore();
    app.submit
}

// ---------- non-TUI helpers ----------

// Returns None on EOF (Ctrl+D) or a read error, so callers can abort instead of
// re-prompting forever — read_line yielding Ok(0) means the stream is closed, not
// that the user submitted an empty line.
fn prompt(label: &str) -> Option<String> {
    print!("{label}: ");
    io::stdout().flush().ok();
    let mut s = String::new();
    match io::stdin().read_line(&mut s) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(s.trim().to_string()),
    }
}

fn collect(wf: &Workflow, flags: &HashMap<String, String>) -> Result<HashMap<String, String>, String> {
    let interactive = io::stdin().is_terminal();
    collect_fields(wf, flags, interactive, prompt)
}

// The prompt source is injected (`ask`) so the resolution logic — flags, defaults,
// EOF abort, required checks, choices validation — is testable without a real stdin/tty.
fn collect_fields(
    wf: &Workflow,
    flags: &HashMap<String, String>,
    interactive: bool,
    mut ask: impl FnMut(&str) -> Option<String>,
) -> Result<HashMap<String, String>, String> {
    let mut values = HashMap::new();
    for f in &wf.fields {
        // choices_command fields are a pick-from-a-known-list even on the non-TUI path:
        // surface the options when prompting and reject anything outside them.
        let choices = match &f.choices_command {
            Some(command) => run_choices(command, &wf.dir),
            None => Vec::new(),
        };
        let label = f.prompt.clone().unwrap_or_else(|| f.name.clone());
        let hint = if choices.is_empty() { String::new() } else { format!(" [{}]", choices.join(", ")) };

        let val = if let Some(v) = flags.get(&f.name) {
            v.clone()
        } else if interactive {
            let aborted = || format!("aborted while reading --{} (EOF)", f.name);
            let mut v = ask(&format!("{label}{hint}")).ok_or_else(aborted)?;
            while f.required && v.is_empty() && f.default.is_none() {
                v = ask(&format!("{label}{hint} (required)")).ok_or_else(aborted)?;
            }
            if v.is_empty() {
                if let Some(d) = &f.default {
                    v = d.clone();
                }
            }
            v
        } else if let Some(d) = &f.default {
            d.clone()
        } else if f.required {
            return Err(format!("missing required field --{} (no TTY for prompt)", f.name));
        } else {
            String::new()
        };

        if !choices.is_empty() && !val.is_empty() && !choices.contains(&val) {
            return Err(format!(
                "invalid value for --{}: '{val}' (choices: {})",
                f.name,
                choices.join(", ")
            ));
        }
        values.insert(f.name.clone(), val);
    }
    Ok(values)
}

fn parse_args(args: &[String]) -> (Vec<String>, HashMap<String, String>) {
    let mut positionals = Vec::new();
    let mut flags = HashMap::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(key) = args[i].strip_prefix("--") {
            if let Some((k, v)) = key.split_once('=') {
                flags.insert(k.to_string(), v.to_string());
            } else if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                flags.insert(key.to_string(), args[i + 1].clone());
                i += 1;
            } else {
                flags.insert(key.to_string(), "true".to_string());
            }
        } else {
            positionals.push(args[i].clone());
        }
        i += 1;
    }
    (positionals, flags)
}

fn dump_snapshot(app: &mut App, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ui(f, app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let area = buf.area;
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        while out.ends_with(' ') {
            out.pop();
        }
        out.push('\n');
    }
    out
}

fn ctx_cwd() -> Option<String> {
    let raw = env::var("HERDR_PLUGIN_CONTEXT_JSON").ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    for key in ["workspace_cwd", "focused_pane_cwd"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let (positionals, flags) = parse_args(&args);
    let cmd = positionals.first().map(String::as_str).unwrap_or("list");
    let workflows = load_workflows();

    match cmd {
        "list" => {
            println!("Available workflows:");
            for w in &workflows {
                println!("  {}\t{}", w.name, w.description);
            }
        }
        "open" => {
            let herdr = env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
            let Ok(id) = env::var("HERDR_PLUGIN_ID") else {
                eprintln!("HERDR_PLUGIN_ID not set — run via the herdr action");
                std::process::exit(1);
            };
            let mut cmd = Command::new(herdr);
            cmd.args(["plugin", "pane", "open", "--plugin", &id, "--entrypoint", "launcher-ui", "--placement", "split", "--direction", "right"]);
            // Carry the invoking workspace's cwd into the overlay pane (its own cwd is
            // the plugin dir), so workflows like new-task target the right repo.
            if let Some(cwd) = ctx_cwd() {
                cmd.args(["--cwd", &cwd]);
            }
            match cmd.status() {
                Ok(s) => std::process::exit(s.code().unwrap_or(0)),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        "run" => {
            let Some(name) = positionals.get(1) else {
                eprintln!("usage: herdr-launcher run <name>");
                std::process::exit(1);
            };
            match workflows.iter().find(|w| &w.name == name) {
                Some(w) => match collect(w, &flags) {
                    Ok(v) => run_workflow(w, &v),
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                },
                None => {
                    eprintln!("unknown workflow: {name}");
                    std::process::exit(1);
                }
            }
        }
        "pick" => {
            if workflows.is_empty() {
                println!("No workflows found. Add .toml files to ~/.config/herdr-launcher/workflows/");
                return;
            }
            if !io::stdout().is_terminal() {
                eprintln!("pick needs a terminal; use `run <name>` without a tty");
                std::process::exit(1);
            }
            if let Some((wf, values)) = run_tui(workflows) {
                run_workflow(&wf, &values);
            }
        }
        "__snapshot" => {
            let mut app = App::new(workflows);
            println!("--- list screen ---");
            print!("{}", dump_snapshot(&mut app, 72, 11));
            app.open_form();
            if let Some(FieldInput::Line(input)) = app.inputs.get_mut(0) {
                *input = Input::new("World".to_string());
            }
            println!("\n--- form screen (first field active) ---");
            print!("{}", dump_snapshot(&mut app, 72, 24));
        }
        other => {
            eprintln!("unknown command: {other} (use: list | run <name> | pick)");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, required: bool) -> Field {
        field_with(name, required, None, None)
    }

    fn field_with(name: &str, required: bool, default: Option<&str>, choices_command: Option<&str>) -> Field {
        Field {
            name: name.to_string(),
            prompt: None,
            required,
            default: default.map(String::from),
            multiline: false,
            choices_command: choices_command.map(String::from),
        }
    }

    fn workflow(name: &str, command: &str, fields: Vec<Field>) -> Workflow {
        Workflow {
            name: name.to_string(),
            description: String::new(),
            fields,
            command: command.to_string(),
            dir: PathBuf::new(),
        }
    }

    #[test]
    fn field_names_must_be_shell_identifiers() {
        assert!(valid_field_name("branch"));
        assert!(valid_field_name("branch_name"));
        assert!(valid_field_name("_x1"));
        assert!(!valid_field_name("branch-name"));
        assert!(!valid_field_name("1branch"));
        assert!(!valid_field_name(""));
        assert!(!valid_field_name("has space"));
    }

    #[test]
    fn parse_args_splits_positionals_and_flags() {
        let args = ["run", "wf", "--x", "1", "--y=2", "--flag"].map(String::from).to_vec();
        let (positionals, flags) = parse_args(&args);
        assert_eq!(positionals, vec!["run", "wf"]);
        assert_eq!(flags.get("x"), Some(&"1".to_string()));
        assert_eq!(flags.get("y"), Some(&"2".to_string()));
        assert_eq!(flags.get("flag"), Some(&"true".to_string()));
    }

    #[test]
    fn visible_window_always_contains_focus() {
        let heights = vec![3u16; 9];
        for focus in 0..heights.len() {
            let (start, end) = visible_window(&heights, focus, 10);
            assert!(start <= focus && focus < end, "focus {focus} not in [{start},{end})");
            let used: u16 = heights[start..end].iter().sum();
            assert!(used <= 10, "window {used} rows exceeds avail");
        }
    }

    #[test]
    fn visible_window_renders_oversized_field_alone() {
        let heights = vec![3, 20, 3];
        let (start, end) = visible_window(&heights, 1, 5);
        assert_eq!((start, end), (1, 2));
    }

    #[test]
    fn duplicate_names_resolve_deterministically() {
        let dir = env::temp_dir().join(format!("hl-dedup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("b.toml"), "name = \"dup\"\ncommand = \"echo b\"\n").unwrap();
        fs::write(dir.join("a.toml"), "name = \"dup\"\ncommand = \"echo a\"\n").unwrap();

        let mut out = Vec::new();
        load_dir(&dir, &mut out);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].command, "echo a", "a.toml sorts first, so it wins deterministically");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn form_keeps_focused_field_visible_in_a_short_pane() {
        let fields: Vec<Field> = (0..9).map(|i| field(&format!("f{i}"), false)).collect();
        let mut app = App::new(vec![workflow("demo", "true", fields)]);
        app.open_form();
        app.field_idx = 8;
        let out = dump_snapshot(&mut app, 60, 14);
        assert!(out.contains("f8"), "last field must be on screen when focused:\n{out}");
    }

    #[test]
    fn required_field_blocks_submit_with_a_message() {
        let mut app = App::new(vec![workflow("demo", "true", vec![field("name", true)])]);
        app.open_form();
        app.try_submit();
        assert!(app.submit.is_none());
        assert!(app.error.is_some(), "empty required field must surface an error");
    }

    #[test]
    fn collect_aborts_on_eof_instead_of_looping() {
        let wf = workflow("demo", "true", vec![field("name", true)]);
        let err = collect_fields(&wf, &HashMap::new(), true, |_| None).unwrap_err();
        assert!(err.contains("EOF"), "EOF on a required field must abort, got: {err}");
    }

    #[test]
    fn collect_reprompts_required_field_then_accepts_value() {
        let wf = workflow("demo", "true", vec![field("name", true)]);
        let mut replies = vec![Some(String::new()), Some("finally".to_string())].into_iter();
        let out = collect_fields(&wf, &HashMap::new(), true, move |_| replies.next().flatten()).unwrap();
        assert_eq!(out.get("name"), Some(&"finally".to_string()));
    }

    #[test]
    fn collect_requires_flag_or_default_without_tty() {
        let wf = workflow("demo", "true", vec![field("name", true)]);
        let err = collect_fields(&wf, &HashMap::new(), false, |_| None).unwrap_err();
        assert!(err.contains("missing required"), "got: {err}");
    }

    #[test]
    fn collect_falls_back_to_default_without_tty() {
        let wf = workflow("demo", "true", vec![field_with("lang", false, Some("en"), None)]);
        let out = collect_fields(&wf, &HashMap::new(), false, |_| None).unwrap();
        assert_eq!(out.get("lang"), Some(&"en".to_string()));
    }

    #[test]
    fn collect_rejects_value_outside_choices() {
        let wf = workflow("demo", "true", vec![field_with("c", true, None, Some("printf 'a\\nb\\n'"))]);
        let mut bad = HashMap::new();
        bad.insert("c".to_string(), "zzz".to_string());
        let err = collect_fields(&wf, &bad, false, |_| None).unwrap_err();
        assert!(err.contains("invalid value"), "got: {err}");

        let mut good = HashMap::new();
        good.insert("c".to_string(), "a".to_string());
        let out = collect_fields(&wf, &good, false, |_| None).unwrap();
        assert_eq!(out.get("c"), Some(&"a".to_string()));
    }

    #[test]
    fn select_clears_value_when_nothing_matches() {
        let mut s = SelectField::new(vec!["alpha".into(), "beta".into()], None);
        assert_eq!(s.value, "alpha", "first choice is committed on open");
        for c in "zzz".chars() {
            s.handle(KeyEvent::from(KeyCode::Char(c)));
        }
        assert!(s.filtered.is_empty());
        assert_eq!(s.value, "", "a zero-match query must not submit a stale choice");
    }

    #[test]
    fn select_defaults_to_the_given_choice() {
        let s = SelectField::new(vec!["a".into(), "b".into(), "c".into()], Some("b".into()));
        assert_eq!(s.value, "b");
    }

    #[test]
    fn open_form_preserves_input_when_reentering_same_workflow() {
        let mut app = App::new(vec![
            workflow("first", "true", vec![field("name", false)]),
            workflow("second", "true", vec![field("other", false)]),
        ]);
        app.selected = 0;
        app.open_form();
        app.inputs[0] = FieldInput::Line(Input::new("typed".to_string()));

        app.screen = Screen::List;
        app.open_form();
        assert_eq!(app.inputs[0].value(), "typed", "re-entering the same workflow keeps input");

        app.selected = 1;
        app.open_form();
        assert_eq!(app.inputs[0].value(), "", "switching workflows rebuilds the form");
    }
}
