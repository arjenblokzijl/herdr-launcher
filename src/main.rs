// herdr-launcher (Rust) — a workflow launcher with a ratatui TUI.
// Workflows are declarative TOML (fields + a command that reads $HERDR_FIELD_<name>).
//   herdr-launcher list
//   herdr-launcher run <name> [--field value ...]
//   herdr-launcher pick            (ratatui picker + field form; falls back to stdin without a tty)
//   herdr-launcher __snapshot      (render the TUI to text via TestBackend — for demos/tests)
// Workflows: *.toml in $HERDR_WORKFLOWS_DIR and ~/.config/herdr-launcher/workflows/.

use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Terminal;
use serde::Deserialize;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;
use tui_textarea::{CursorMove, TextArea};

const CONFIG_ERROR_ENV: &str = "HERDR_LAUNCHER_CONFIG_ERROR";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Placement {
    #[default]
    Split,
    Overlay,
}

impl Placement {
    fn as_str(self) -> &'static str {
        match self {
            Placement::Split => "split",
            Placement::Overlay => "overlay",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct LauncherConfig {
    #[serde(default)]
    placement: Placement,
    #[serde(default)]
    workflows: HashMap<String, HashMap<String, String>>,
}

#[derive(Clone, Debug)]
struct LauncherConfigError {
    path: PathBuf,
    detail: String,
}

impl fmt::Display for LauncherConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid Launcher config at {}: {}",
            self.path.display(),
            self.detail
        )
    }
}

fn config_path(plugin_dir: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    plugin_dir
        .map(|dir| dir.join("config.toml"))
        .or_else(|| home.map(|dir| dir.join(".config/herdr-launcher/config.toml")))
}

fn launcher_config_path() -> Option<PathBuf> {
    let plugin_dir = env::var_os("HERDR_PLUGIN_CONFIG_DIR").map(PathBuf::from);
    let home = env::var_os("HOME").map(PathBuf::from);
    config_path(plugin_dir.as_deref(), home.as_deref())
}

fn load_config_from(path: Option<&Path>) -> Result<LauncherConfig, LauncherConfigError> {
    let Some(path) = path else {
        return Ok(LauncherConfig::default());
    };
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(LauncherConfig::default());
        }
        Err(error) => {
            return Err(LauncherConfigError {
                path: path.to_path_buf(),
                detail: format!("cannot read file: {error}"),
            });
        }
    };
    let config = toml::from_str(&text).map_err(|error| LauncherConfigError {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;
    validate_workflow_config(&config).map_err(|detail| LauncherConfigError {
        path: path.to_path_buf(),
        detail,
    })?;
    Ok(config)
}

fn valid_config_key(key: &str) -> bool {
    valid_field_name(key) && !key.bytes().any(|byte| byte.is_ascii_uppercase())
}

fn validate_workflow_config(config: &LauncherConfig) -> Result<(), String> {
    for (namespace, values) in &config.workflows {
        let mut projected = HashSet::new();
        for key in values.keys() {
            let environment_key = key.to_ascii_uppercase();
            if !projected.insert(environment_key.clone()) {
                return Err(format!(
                    "workflow config keys in namespace '{namespace}' collide as HERDR_WORKFLOW_CONFIG_{environment_key}"
                ));
            }
        }
        for key in values.keys() {
            if !valid_config_key(key) {
                return Err(format!(
                    "workflow config key '{key}' in namespace '{namespace}' must be a lowercase ASCII shell identifier"
                ));
            }
        }
    }
    Ok(())
}

fn load_config() -> Result<LauncherConfig, LauncherConfigError> {
    let path = launcher_config_path();
    load_config_from(path.as_deref())
}

fn sanitize_env_value(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn pane_open_args(
    plugin_id: &str,
    placement: Placement,
    cwd: Option<&str>,
    config_error: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "plugin".to_string(),
        "pane".to_string(),
        "open".to_string(),
        "--plugin".to_string(),
        plugin_id.to_string(),
        "--entrypoint".to_string(),
        "launcher-ui".to_string(),
        "--placement".to_string(),
        placement.as_str().to_string(),
    ];
    if let Some(cwd) = cwd {
        args.extend(["--cwd".to_string(), cwd.to_string()]);
    }
    if let Some(error) = config_error {
        args.extend([
            "--env".to_string(),
            format!("{CONFIG_ERROR_ENV}={}", sanitize_env_value(error)),
        ]);
    }
    args
}

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
    #[serde(default)]
    depends_on: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum WorkflowAvailability {
    #[default]
    Available,
    Disabled(String),
}

#[derive(Deserialize, Clone)]
struct Workflow {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    availability_command: Option<String>,
    #[serde(default)]
    config_namespace: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    fields: Vec<Field>,
    command: String,
    #[serde(skip)]
    dir: PathBuf,
    #[serde(skip)]
    config: HashMap<String, String>,
    #[serde(skip)]
    availability: WorkflowAvailability,
}

impl Workflow {
    fn label(&self) -> &str {
        self.title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(&self.name)
    }
}

fn cli_workflow_label(workflow: &Workflow) -> String {
    if workflow.label() == workflow.name {
        workflow.name.clone()
    } else {
        format!("{} [{}]", workflow.label(), workflow.name)
    }
}

fn workflow_list_text(workflow: &Workflow) -> String {
    match &workflow.availability {
        WorkflowAvailability::Available => {
            format!("{}  —  {}", workflow.label(), workflow.description)
        }
        WorkflowAvailability::Disabled(reason) => {
            format!("[disabled] {}  —  {reason}", workflow.label())
        }
    }
}

fn ensure_workflow_available(workflow: &Workflow) -> Result<(), String> {
    match &workflow.availability {
        WorkflowAvailability::Available => Ok(()),
        WorkflowAvailability::Disabled(reason) => Err(format!(
            "workflow '{}' is disabled: {reason}",
            workflow.name
        )),
    }
}

fn sort_workflows(workflows: &mut [Workflow]) {
    workflows.sort_by(|left, right| {
        left.label()
            .cmp(right.label())
            .then_with(|| left.name.cmp(&right.name))
    });
}

fn workflow_by_name<'a>(workflows: &'a [Workflow], name: &str) -> Option<&'a Workflow> {
    workflows.iter().find(|workflow| workflow.name == name)
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
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
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

fn validate_field_dependencies(workflow: &Workflow) -> Result<(), String> {
    for (index, field) in workflow.fields.iter().enumerate() {
        if !field.depends_on.is_empty() && field.choices_command.is_none() {
            return Err(format!(
                "field '{}' declares depends_on but has no choices_command",
                field.name
            ));
        }
        for dependency in &field.depends_on {
            if !workflow.fields[..index]
                .iter()
                .any(|candidate| candidate.name == *dependency)
            {
                return Err(format!(
                    "field '{}' dependency '{}' must name an earlier field",
                    field.name, dependency
                ));
            }
        }
    }
    Ok(())
}

fn validate_workflow(workflow: &Workflow) -> Result<(), String> {
    if let Some(field) = workflow
        .fields
        .iter()
        .find(|field| !valid_field_name(&field.name))
    {
        return Err(format!(
            "field name '{}' is not a valid shell identifier ([A-Za-z_][A-Za-z0-9_]*)",
            field.name
        ));
    }
    if workflow.config_namespace.as_deref() == Some("") {
        return Err("config_namespace must not be empty".to_string());
    }
    validate_field_dependencies(workflow)
}

// A workflow's `dir` is where its .toml lives, exposed to the command and
// choices_command as $HERDR_WORKFLOW_DIR so a workflow can reference its own
// bundled helper scripts relocatably.
fn load_toml_file(path: &Path, dir: &Path, out: &mut Vec<Workflow>) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    match toml::from_str::<Workflow>(&text) {
        Ok(mut w) if !out.iter().any(|x| x.name == w.name) => {
            if let Err(error) = validate_workflow(&w) {
                eprintln!("skip {}: {error}", path.display());
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

fn workflow_config_namespace(workflow: &Workflow) -> &str {
    workflow
        .config_namespace
        .as_deref()
        .unwrap_or(&workflow.name)
}

fn apply_workflow_config(workflow: &mut Workflow, config: &LauncherConfig) {
    let namespace = workflow_config_namespace(workflow).to_string();
    workflow.config = config
        .workflows
        .get(&namespace)
        .cloned()
        .unwrap_or_default();
}

fn availability_from_output(output: io::Result<Output>) -> WorkflowAvailability {
    match output {
        Ok(output) if output.status.success() => WorkflowAvailability::Available,
        Ok(output) => {
            let reason = sanitize_env_value(String::from_utf8_lossy(&output.stderr).trim());
            if reason.is_empty() {
                let status = output
                    .status
                    .code()
                    .map(|code| format!("exit {code}"))
                    .unwrap_or_else(|| "signal".to_string());
                WorkflowAvailability::Disabled(format!("availability check failed ({status})"))
            } else {
                WorkflowAvailability::Disabled(reason)
            }
        }
        Err(error) => WorkflowAvailability::Disabled(format!(
            "availability check could not start: {}",
            sanitize_env_value(&error.to_string())
        )),
    }
}

fn evaluate_workflow_availability(workflow: &mut Workflow) {
    let Some(command) = workflow.availability_command.clone() else {
        workflow.availability = WorkflowAvailability::Available;
        return;
    };
    let output = shell_command(
        &command,
        workflow,
        workflow_environment(workflow, &HashMap::new()),
    )
    .stdout(Stdio::null())
    .output();
    workflow.availability = availability_from_output(output);
}

fn evaluate_workflow_availabilities(workflows: &mut [Workflow]) {
    for workflow in workflows {
        evaluate_workflow_availability(workflow);
    }
}

fn load_workflows(config: &LauncherConfig) -> Vec<Workflow> {
    let mut out: Vec<Workflow> = Vec::new();
    for dir in workflow_dirs() {
        load_dir(&dir, &mut out);
    }
    for workflow in &mut out {
        apply_workflow_config(workflow, config);
    }
    sort_workflows(&mut out);
    out
}

fn workflow_environment(
    workflow: &Workflow,
    values: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut environment = HashMap::new();
    for (key, value) in &workflow.config {
        environment.insert(
            format!("HERDR_WORKFLOW_CONFIG_{}", key.to_ascii_uppercase()),
            value.clone(),
        );
    }
    for (key, value) in values {
        environment.insert(format!("HERDR_FIELD_{key}"), value.clone());
    }
    environment
}

fn choice_environment(
    workflow: &Workflow,
    field: &Field,
    values: &HashMap<String, String>,
) -> HashMap<String, String> {
    let dependencies = field
        .depends_on
        .iter()
        .filter_map(|name| values.get(name).map(|value| (name.clone(), value.clone())))
        .collect();
    workflow_environment(workflow, &dependencies)
}

fn is_launcher_environment(key: &OsStr) -> bool {
    let key = key.to_string_lossy();
    key.starts_with("HERDR_FIELD_") || key.starts_with("HERDR_WORKFLOW_CONFIG_")
}

fn shell_command(
    script: &str,
    workflow: &Workflow,
    environment: HashMap<String, String>,
) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(script);
    command.env_clear();
    command.envs(env::vars_os().filter(|(key, _)| !is_launcher_environment(key)));
    command.env("HERDR_WORKFLOW_DIR", &workflow.dir);
    command.envs(environment);
    command
}

fn run_workflow(wf: &Workflow, values: &HashMap<String, String>) -> ! {
    // The command reads field values from $HERDR_FIELD_<name> (and $HERDR_WORKFLOW_DIR
    // for bundled scripts). Values are passed via the environment, never interpolated
    // into the command string, so a field containing shell metacharacters can't inject.
    let mut command = shell_command(&wf.command, wf, workflow_environment(wf, values));
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
    StartupError,
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
    dependency_signature: Option<Vec<(String, String)>>,
    dirty: bool,
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
            dependency_signature: None,
            dirty: false,
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

    fn pending() -> Self {
        let mut select = Self::new(Vec::new(), None);
        select.dirty = true;
        select
    }

    fn replace_choices(&mut self, choices: Vec<String>, default: Option<&str>) {
        let previous = self.value.clone();
        self.choices = choices;
        self.query = Input::default();
        self.sel = 0;
        self.refilter();
        let preferred = self
            .filtered
            .iter()
            .position(|choice| *choice == previous)
            .or_else(|| {
                default.and_then(|value| self.filtered.iter().position(|choice| choice == value))
            });
        if let Some(position) = preferred {
            self.sel = position;
        }
        self.commit();
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

fn dependency_signature(field: &Field, values: &HashMap<String, String>) -> Vec<(String, String)> {
    field
        .depends_on
        .iter()
        .map(|name| (name.clone(), values.get(name).cloned().unwrap_or_default()))
        .collect()
}

fn run_choices(
    field: &Field,
    workflow: &Workflow,
    values: &HashMap<String, String>,
) -> Result<Vec<String>, String> {
    let Some(command) = field.choices_command.as_deref() else {
        return Ok(Vec::new());
    };
    let output = shell_command(
        command,
        workflow,
        choice_environment(workflow, field, values),
    )
    .output()
    .map_err(|error| {
        format!(
            "failed to start choices command for '{}': {error}",
            field.name
        )
    })?;
    if !output.status.success() {
        let detail = sanitize_env_value(String::from_utf8_lossy(&output.stderr).trim());
        let status = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string());
        return Err(if detail.is_empty() {
            format!("choices command for '{}' failed ({status})", field.name)
        } else {
            format!(
                "choices command for '{}' failed ({status}): {detail}",
                field.name
            )
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .filter(|line| !line.is_empty())
        .collect())
}

fn build_input(field: &Field) -> FieldInput {
    let default = field.default.clone().unwrap_or_default();
    if field.choices_command.is_some() {
        // Always a Select (even with no candidates) so a choices_command field stays
        // constrained — never silently downgraded to unvalidated free text.
        return FieldInput::Select(Box::new(SelectField::pending()));
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
    startup_error: Option<String>,
    selected: usize,
    screen: Screen,
    form_idx: usize,         // index into workflows of the workflow being filled
    inputs: Vec<FieldInput>, // current editor per field
    field_idx: usize,
    has_form: bool, // whether `inputs` holds a form for `form_idx`
    error: Option<String>,
    loading_choices: bool,
    submit: Option<(Workflow, HashMap<String, String>)>,
}

impl App {
    fn new(workflows: Vec<Workflow>) -> Self {
        App {
            workflows,
            startup_error: None,
            selected: 0,
            screen: Screen::List,
            form_idx: 0,
            inputs: vec![],
            field_idx: 0,
            has_form: false,
            error: None,
            loading_choices: false,
            submit: None,
        }
    }

    fn with_startup_error(error: String) -> Self {
        let mut app = Self::new(Vec::new());
        app.startup_error = Some(error);
        app.screen = Screen::StartupError;
        app
    }

    fn move_list(&mut self, delta: isize) {
        let n = self.workflows.len() as isize;
        if n == 0 {
            return;
        }
        self.selected = (((self.selected as isize) + delta).rem_euclid(n)) as usize;
        self.error = None;
    }

    fn prepare_form(&mut self) -> bool {
        if self.workflows.is_empty() {
            return false;
        }
        if let Err(error) = ensure_workflow_available(&self.workflows[self.selected]) {
            self.error = Some(error);
            return false;
        }
        // Rebuild only when entering a different workflow. Re-opening the same one
        // (e.g. after Esc to peek at the list) keeps whatever was already typed.
        if !self.has_form || self.form_idx != self.selected {
            self.form_idx = self.selected;
            let wf = &self.workflows[self.form_idx];
            self.inputs = wf.fields.iter().map(build_input).collect();
            self.field_idx = 0;
            self.has_form = true;
        } else {
            for input in &mut self.inputs {
                if let FieldInput::Select(select) = input {
                    select.dirty = true;
                }
            }
        }
        self.error = None;
        self.screen = Screen::Form;
        self.loading_choices = self.workflows[self.form_idx]
            .fields
            .iter()
            .any(|field| field.choices_command.is_some());
        true
    }

    fn refresh_form_choices(&mut self) {
        for index in 0..self.inputs.len() {
            if let Err(error) = self.refresh_choice(index) {
                self.field_idx = index;
                self.error = Some(error);
                break;
            }
        }
        self.loading_choices = false;
    }

    fn open_form(&mut self) {
        if self.prepare_form() {
            self.refresh_form_choices();
        }
    }

    fn move_field(&mut self, delta: isize) {
        let n = self.workflows[self.form_idx].fields.len() as isize;
        if n == 0 {
            return;
        }
        self.field_idx = (((self.field_idx as isize) + delta).rem_euclid(n)) as usize;
    }

    fn choice_needs_refresh(&self, index: usize) -> bool {
        let workflow = &self.workflows[self.form_idx];
        let field = &workflow.fields[index];
        if field.choices_command.is_none() {
            return false;
        }
        let signature = dependency_signature(field, &self.form_values());
        matches!(
            self.inputs.get(index),
            Some(FieldInput::Select(select))
                if select.dirty || select.dependency_signature.as_ref() != Some(&signature)
        )
    }

    fn choices_need_refresh(&self) -> bool {
        (0..self.inputs.len()).any(|index| self.choice_needs_refresh(index))
    }

    fn refresh_current_choice(&mut self) {
        if let Err(error) = self.refresh_choice(self.field_idx) {
            self.error = Some(error);
        }
    }

    fn current_multiline(&self) -> bool {
        matches!(self.inputs.get(self.field_idx), Some(FieldInput::Area(_)))
    }

    fn form_values(&self) -> HashMap<String, String> {
        self.workflows[self.form_idx]
            .fields
            .iter()
            .zip(&self.inputs)
            .map(|(field, input)| (field.name.clone(), input.value()))
            .collect()
    }

    fn mark_dependents_dirty(&mut self, changed_index: usize) {
        let changed_name = self.workflows[self.form_idx].fields[changed_index]
            .name
            .clone();
        for (index, field) in self.workflows[self.form_idx]
            .fields
            .iter()
            .enumerate()
            .skip(changed_index + 1)
        {
            if field.depends_on.contains(&changed_name) {
                if let Some(FieldInput::Select(select)) = self.inputs.get_mut(index) {
                    select.dirty = true;
                }
            }
        }
    }

    fn handle_current_input(&mut self, key: KeyEvent) {
        let Some(input) = self.inputs.get_mut(self.field_idx) else {
            return;
        };
        let before = input.value();
        input.handle(key);
        if input.value() != before {
            self.mark_dependents_dirty(self.field_idx);
        }
    }

    fn refresh_choice(&mut self, index: usize) -> Result<(), String> {
        if self.workflows[self.form_idx].fields[index]
            .choices_command
            .is_none()
        {
            return Ok(());
        }
        let workflow = self.workflows[self.form_idx].clone();
        let field = workflow.fields[index].clone();
        let values = self.form_values();
        let signature = dependency_signature(&field, &values);
        let needs_refresh = match self.inputs.get(index) {
            Some(FieldInput::Select(select)) => {
                select.dirty || select.dependency_signature.as_ref() != Some(&signature)
            }
            _ => false,
        };
        if !needs_refresh {
            return Ok(());
        }

        let before = self.inputs[index].value();
        let has_unresolved_required_dependency = field.depends_on.iter().any(|name| {
            workflow
                .fields
                .iter()
                .find(|candidate| candidate.name == *name)
                .is_some_and(|dependency| {
                    dependency.required
                        && values.get(name).is_none_or(|value| value.trim().is_empty())
                })
        });
        if has_unresolved_required_dependency {
            let select = match &mut self.inputs[index] {
                FieldInput::Select(select) => select,
                _ => return Ok(()),
            };
            select.dependency_signature = Some(signature);
            select.replace_choices(Vec::new(), None);
            select.dirty = false;
            if before != select.value {
                self.mark_dependents_dirty(index);
            }
            return Ok(());
        }

        let result = run_choices(&field, &workflow, &values);
        let select = match &mut self.inputs[index] {
            FieldInput::Select(select) => select,
            _ => return Ok(()),
        };
        select.dependency_signature = Some(signature);
        match result {
            Ok(choices) => {
                select.replace_choices(choices, field.default.as_deref());
                select.dirty = false;
            }
            Err(error) => {
                select.replace_choices(Vec::new(), None);
                select.dirty = true;
                if before != select.value {
                    self.mark_dependents_dirty(index);
                }
                return Err(error);
            }
        }
        if before != select.value {
            self.mark_dependents_dirty(index);
        }
        Ok(())
    }

    fn try_submit(&mut self) {
        self.submit = None;
        for index in 0..self.inputs.len() {
            if let Err(error) = self.refresh_choice(index) {
                self.field_idx = index;
                self.error = Some(error);
                return;
            }
        }
        let wf = self.workflows[self.form_idx].clone();
        let mut values: Vec<String> = self.inputs.iter().map(FieldInput::value).collect();
        for (i, f) in wf.fields.iter().enumerate() {
            if values[i].trim().is_empty()
                && !matches!(self.inputs.get(i), Some(FieldInput::Select(_)))
            {
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
        Screen::StartupError => {
            let detail = app
                .startup_error
                .as_deref()
                .unwrap_or("invalid Launcher configuration");
            let rows = Layout::vertical([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);
            f.render_widget(
                Paragraph::new(" Launcher configuration error").style(Style::new().red().bold()),
                rows[0],
            );
            f.render_widget(
                Paragraph::new(format!(" {detail}"))
                    .wrap(Wrap { trim: false })
                    .style(Style::new().red()),
                rows[1],
            );
            f.render_widget(Paragraph::new(Line::from(" Esc/q close").dim()), rows[2]);
        }
        Screen::List => {
            // No inner border — herdr already draws the pane frame ("Launcher").
            let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
            let items: Vec<ListItem> = app
                .workflows
                .iter()
                .map(|workflow| {
                    let item = ListItem::new(format!(" {}", workflow_list_text(workflow)));
                    if matches!(workflow.availability, WorkflowAvailability::Disabled(_)) {
                        item.style(Style::new().dim())
                    } else {
                        item
                    }
                })
                .collect();
            let list = List::new(items)
                .highlight_symbol("▶ ")
                .highlight_style(Style::new().bold().fg(Color::Cyan));
            let mut state = ListState::default();
            state.select(Some(app.selected));
            f.render_stateful_widget(list, rows[0], &mut state);
            let footer = if let Some(error) = app.error.as_deref() {
                Line::from(format!(" {error}")).style(Style::new().red().bold())
            } else {
                Line::from(" ↑↓ move · Enter select · Esc close").dim()
            };
            f.render_widget(Paragraph::new(footer), rows[1]);
        }
        Screen::Form => {
            let App {
                workflows,
                form_idx,
                field_idx,
                inputs,
                error,
                loading_choices,
                ..
            } = app;
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
                    Line::from(format!(" {}", wf.label())).bold(),
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
                    .border_type(if focused {
                        BorderType::Thick
                    } else {
                        BorderType::Plain
                    })
                    .border_style(if focused {
                        Style::new().cyan()
                    } else {
                        Style::new().dim()
                    })
                    .title(if focused {
                        Span::from(title).bold().cyan()
                    } else {
                        Span::from(title).dim()
                    });

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
                            Paragraph::new(input.value())
                                .scroll((0, scroll as u16))
                                .block(block),
                            rect,
                        );
                        if focused {
                            let cx =
                                rect.x + 1 + (input.visual_cursor().max(scroll) - scroll) as u16;
                            f.set_cursor_position((cx, rect.y + 1));
                        }
                    }
                    FieldInput::Select(select) => {
                        if !focused {
                            f.render_widget(
                                Paragraph::new(select.value.as_str()).block(block),
                                rect,
                            );
                            continue;
                        }
                        let block = block.title_bottom(
                            Line::from(format!(" {} matches ", select.filtered.len()))
                                .dim()
                                .right_aligned(),
                        );
                        let inner = block.inner(rect);
                        f.render_widget(block, rect);
                        let [search, list] =
                            Layout::vertical([Constraint::Length(1), Constraint::Min(1)])
                                .areas(inner);

                        let prefix = "› ";
                        f.render_widget(
                            Paragraph::new(format!("{prefix}{}", select.query.value())),
                            search,
                        );
                        let cx = search.x
                            + prefix.chars().count() as u16
                            + select.query.visual_cursor() as u16;
                        f.set_cursor_position((cx, search.y));

                        let items: Vec<ListItem> = select
                            .filtered
                            .iter()
                            .map(|c| ListItem::new(c.as_str()))
                            .collect();
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
            let footer = if *loading_choices {
                Line::from(" Loading choices…").style(Style::new().cyan().bold())
            } else if let Some(err) = error.as_deref() {
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

fn run_app_tui(mut app: App) -> Option<(Workflow, HashMap<String, String>)> {
    let mut terminal = ratatui::init();
    let _ = terminal.clear();
    loop {
        let _ = terminal.draw(|f| ui(f, &mut app));
        let Ok(Event::Key(key)) = event::read() else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match app.screen {
            Screen::StartupError => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                _ => {}
            },
            Screen::List => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Down | KeyCode::Char('j') => app.move_list(1),
                KeyCode::Up | KeyCode::Char('k') => app.move_list(-1),
                KeyCode::Enter if app.prepare_form() => {
                    let _ = terminal.draw(|f| ui(f, &mut app));
                    app.refresh_form_choices();
                }
                KeyCode::Enter => {}
                _ => {}
            },
            Screen::Form => {
                app.error = None;
                let multiline = app.current_multiline();
                let mut refresh_current = false;
                let mut submit = false;
                match key.code {
                    KeyCode::Esc => app.screen = Screen::List,
                    KeyCode::Tab => {
                        app.move_field(1);
                        refresh_current = true;
                    }
                    KeyCode::BackTab => {
                        app.move_field(-1);
                        refresh_current = true;
                    }
                    KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        submit = true;
                    }
                    KeyCode::Enter if !multiline => {
                        let last = app.field_idx + 1 >= app.workflows[app.form_idx].fields.len();
                        if last {
                            submit = true;
                        } else {
                            app.move_field(1);
                            refresh_current = true;
                        }
                    }
                    _ if !app.inputs.is_empty() => app.handle_current_input(key),
                    _ => {}
                }
                let show_loading = (submit && app.choices_need_refresh())
                    || (refresh_current && app.choice_needs_refresh(app.field_idx));
                if show_loading {
                    app.loading_choices = true;
                    let _ = terminal.draw(|frame| ui(frame, &mut app));
                }
                if submit {
                    app.try_submit();
                } else if refresh_current {
                    app.refresh_current_choice();
                }
                app.loading_choices = false;
            }
        }
        if app.submit.is_some() {
            break;
        }
    }
    ratatui::restore();
    app.submit
}

fn run_tui(workflows: Vec<Workflow>) -> Option<(Workflow, HashMap<String, String>)> {
    run_app_tui(App::new(workflows))
}

fn run_startup_error_tui(error: String) {
    let _ = run_app_tui(App::with_startup_error(error));
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
        Ok(_) => Some(s.trim_end_matches(['\r', '\n']).to_string()),
    }
}

fn collect(
    wf: &Workflow,
    flags: &HashMap<String, String>,
) -> Result<HashMap<String, String>, String> {
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
            Some(_) => Some(run_choices(f, wf, &values)?),
            None => None,
        };
        let label = f.prompt.clone().unwrap_or_else(|| f.name.clone());
        let hint = if choices.as_ref().is_none_or(Vec::is_empty) {
            String::new()
        } else {
            format!(" [{}]", choices.as_ref().unwrap().join(", "))
        };

        if f.required && choices.as_ref().is_some_and(Vec::is_empty) {
            return Err(format!("no choices available for --{}", f.name));
        }

        let val = if let Some(v) = flags.get(&f.name) {
            v.clone()
        } else if interactive {
            let aborted = || format!("aborted while reading --{} (EOF)", f.name);
            let normalize = |value: String| {
                if choices.is_some() {
                    value
                } else {
                    value.trim().to_string()
                }
            };
            let mut v = normalize(ask(&format!("{label}{hint}")).ok_or_else(aborted)?);
            while f.required && v.is_empty() && f.default.is_none() {
                v = normalize(ask(&format!("{label}{hint} (required)")).ok_or_else(aborted)?);
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
            return Err(format!(
                "missing required field --{} (no TTY for prompt)",
                f.name
            ));
        } else {
            String::new()
        };

        if let Some(choices) = choices {
            if !val.is_empty() && !choices.contains(&val) {
                let allowed = if choices.is_empty() {
                    "no choices available".to_string()
                } else {
                    format!("choices: {}", choices.join(", "))
                };
                return Err(format!(
                    "invalid value for --{}: '{val}' ({allowed})",
                    f.name
                ));
            }
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

    match cmd {
        "list" => {
            let config = load_config().unwrap_or_else(|error| {
                eprintln!("{error}");
                std::process::exit(1);
            });
            let mut workflows = load_workflows(&config);
            evaluate_workflow_availabilities(&mut workflows);
            println!("Available workflows:");
            for w in &workflows {
                match &w.availability {
                    WorkflowAvailability::Available => {
                        println!("  {}\t{}", cli_workflow_label(w), w.description);
                    }
                    WorkflowAvailability::Disabled(reason) => {
                        println!("  [disabled] {}\t{reason}", cli_workflow_label(w));
                    }
                }
            }
        }
        "open" => {
            let herdr = env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
            let Ok(id) = env::var("HERDR_PLUGIN_ID") else {
                eprintln!("HERDR_PLUGIN_ID not set — run via the herdr action");
                std::process::exit(1);
            };
            let (placement, config_error) = match load_config() {
                Ok(config) => (config.placement, None),
                Err(error) => (Placement::Split, Some(error.to_string())),
            };
            let cwd = ctx_cwd();
            let args = pane_open_args(&id, placement, cwd.as_deref(), config_error.as_deref());
            let mut cmd = Command::new(herdr);
            cmd.args(args);
            match cmd.status() {
                Ok(s) => std::process::exit(s.code().unwrap_or(0)),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        "run" => {
            let config = load_config().unwrap_or_else(|error| {
                eprintln!("{error}");
                std::process::exit(1);
            });
            let workflows = load_workflows(&config);
            let Some(name) = positionals.get(1) else {
                eprintln!("usage: herdr-launcher run <name>");
                std::process::exit(1);
            };
            match workflow_by_name(&workflows, name).cloned() {
                Some(mut workflow) => {
                    evaluate_workflow_availability(&mut workflow);
                    if let Err(error) = ensure_workflow_available(&workflow) {
                        eprintln!("{error}");
                        std::process::exit(1);
                    }
                    match collect(&workflow, &flags) {
                        Ok(v) => run_workflow(&workflow, &v),
                        Err(e) => {
                            eprintln!("{e}");
                            std::process::exit(1);
                        }
                    }
                }
                None => {
                    eprintln!("unknown workflow: {name}");
                    std::process::exit(1);
                }
            }
        }
        "pick" => {
            if let Ok(error) = env::var(CONFIG_ERROR_ENV) {
                if !io::stdout().is_terminal() {
                    eprintln!("{error}");
                    std::process::exit(1);
                }
                run_startup_error_tui(error);
                return;
            }
            let config = match load_config() {
                Ok(config) => config,
                Err(error) => {
                    if !io::stdout().is_terminal() {
                        eprintln!("{error}");
                        std::process::exit(1);
                    }
                    run_startup_error_tui(error.to_string());
                    return;
                }
            };
            if !io::stdout().is_terminal() {
                eprintln!("pick needs a terminal; use `run <name>` without a tty");
                std::process::exit(1);
            }
            let mut workflows = load_workflows(&config);
            if workflows.is_empty() {
                println!(
                    "No workflows found. Add .toml files to ~/.config/herdr-launcher/workflows/"
                );
                return;
            }
            evaluate_workflow_availabilities(&mut workflows);
            if let Some((wf, values)) = run_tui(workflows) {
                run_workflow(&wf, &values);
            }
        }
        "__snapshot" => {
            let mut workflows = load_workflows(&LauncherConfig::default());
            evaluate_workflow_availabilities(&mut workflows);
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

    fn field_with(
        name: &str,
        required: bool,
        default: Option<&str>,
        choices_command: Option<&str>,
    ) -> Field {
        Field {
            name: name.to_string(),
            prompt: None,
            required,
            default: default.map(String::from),
            multiline: false,
            choices_command: choices_command.map(String::from),
            depends_on: Vec::new(),
        }
    }

    fn workflow(name: &str, command: &str, fields: Vec<Field>) -> Workflow {
        Workflow {
            name: name.to_string(),
            title: None,
            availability_command: None,
            description: String::new(),
            fields,
            command: command.to_string(),
            config_namespace: None,
            dir: PathBuf::new(),
            config: HashMap::new(),
            availability: WorkflowAvailability::Available,
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
    fn workflow_title_is_an_optional_display_label() {
        let mut workflow = workflow("stable-id", "true", Vec::new());
        assert_eq!(workflow.label(), "stable-id");
        assert_eq!(cli_workflow_label(&workflow), "stable-id");

        workflow.title = Some("Friendly title".into());
        assert_eq!(workflow.label(), "Friendly title");
        assert_eq!(workflow.name, "stable-id");
        assert_eq!(cli_workflow_label(&workflow), "Friendly title [stable-id]");

        workflow.title = Some("   ".into());
        assert_eq!(workflow.label(), "stable-id");
    }

    #[test]
    fn workflow_title_renders_in_the_picker() {
        let mut workflow = workflow("stable-id", "true", Vec::new());
        workflow.title = Some("Friendly title".into());
        let mut app = App::new(vec![workflow]);
        let out = dump_snapshot(&mut app, 60, 8);
        assert!(out.contains("Friendly title"), "{out}");
        assert!(!out.contains("stable-id"), "{out}");
    }

    #[test]
    fn workflow_lookup_uses_the_stable_name() {
        let mut workflow = workflow("stable-id", "true", Vec::new());
        workflow.title = Some("Friendly title".into());
        let workflows = vec![workflow];

        assert!(workflow_by_name(&workflows, "stable-id").is_some());
        assert!(workflow_by_name(&workflows, "Friendly title").is_none());
    }

    #[test]
    fn workflows_sort_by_display_label_then_stable_name() {
        let mut zebra = workflow("a-id", "true", Vec::new());
        zebra.title = Some("Zebra".into());
        let mut alpha_b = workflow("b-id", "true", Vec::new());
        alpha_b.title = Some("Alpha".into());
        let mut alpha_a = workflow("a-alpha-id", "true", Vec::new());
        alpha_a.title = Some("Alpha".into());
        let mut workflows = vec![zebra, alpha_b, alpha_a];

        sort_workflows(&mut workflows);

        let names: Vec<&str> = workflows
            .iter()
            .map(|workflow| workflow.name.as_str())
            .collect();
        assert_eq!(names, ["a-alpha-id", "b-id", "a-id"]);
    }

    #[test]
    fn parse_args_splits_positionals_and_flags() {
        let args = ["run", "wf", "--x", "1", "--y=2", "--flag"]
            .map(String::from)
            .to_vec();
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
            assert!(
                start <= focus && focus < end,
                "focus {focus} not in [{start},{end})"
            );
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
        assert_eq!(
            out[0].command, "echo a",
            "a.toml sorts first, so it wins deterministically"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn form_keeps_focused_field_visible_in_a_short_pane() {
        let fields: Vec<Field> = (0..9).map(|i| field(&format!("f{i}"), false)).collect();
        let mut app = App::new(vec![workflow("demo", "true", fields)]);
        app.open_form();
        app.field_idx = 8;
        let out = dump_snapshot(&mut app, 60, 14);
        assert!(
            out.contains("f8"),
            "last field must be on screen when focused:\n{out}"
        );
    }

    #[test]
    fn required_field_blocks_submit_with_a_message() {
        let mut app = App::new(vec![workflow("demo", "true", vec![field("name", true)])]);
        app.open_form();
        app.try_submit();
        assert!(app.submit.is_none());
        assert!(
            app.error.is_some(),
            "empty required field must surface an error"
        );
    }

    #[test]
    fn collect_aborts_on_eof_instead_of_looping() {
        let wf = workflow("demo", "true", vec![field("name", true)]);
        let err = collect_fields(&wf, &HashMap::new(), true, |_| None).unwrap_err();
        assert!(
            err.contains("EOF"),
            "EOF on a required field must abort, got: {err}"
        );
    }

    #[test]
    fn collect_reprompts_required_field_then_accepts_value() {
        let wf = workflow("demo", "true", vec![field("name", true)]);
        let mut replies = vec![Some(String::new()), Some("finally".to_string())].into_iter();
        let out = collect_fields(&wf, &HashMap::new(), true, move |_| {
            replies.next().flatten()
        })
        .unwrap();
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
        let wf = workflow(
            "demo",
            "true",
            vec![field_with("lang", false, Some("en"), None)],
        );
        let out = collect_fields(&wf, &HashMap::new(), false, |_| None).unwrap();
        assert_eq!(out.get("lang"), Some(&"en".to_string()));
    }

    #[test]
    fn collect_rejects_value_outside_choices() {
        let wf = workflow(
            "demo",
            "true",
            vec![field_with("c", true, None, Some("printf 'a\\nb\\n'"))],
        );
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
    fn collect_preserves_space_padded_interactive_choices() {
        let wf = workflow(
            "demo",
            "true",
            vec![field_with("choice", true, None, Some("printf ' repo \\n'"))],
        );

        let out =
            collect_fields(&wf, &HashMap::new(), true, |_| Some(" repo ".to_string())).unwrap();

        assert_eq!(out.get("choice").map(String::as_str), Some(" repo "));
    }

    #[test]
    fn collect_still_trims_interactive_free_text() {
        let wf = workflow("demo", "true", vec![field("name", true)]);

        let out = collect_fields(&wf, &HashMap::new(), true, |_| {
            Some("  Jeremy  ".to_string())
        })
        .unwrap();

        assert_eq!(out.get("name").map(String::as_str), Some("Jeremy"));
    }

    #[test]
    fn select_clears_value_when_nothing_matches() {
        let mut s = SelectField::new(vec!["alpha".into(), "beta".into()], None);
        assert_eq!(s.value, "alpha", "first choice is committed on open");
        for c in "zzz".chars() {
            s.handle(KeyEvent::from(KeyCode::Char(c)));
        }
        assert!(s.filtered.is_empty());
        assert_eq!(
            s.value, "",
            "a zero-match query must not submit a stale choice"
        );
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
        assert_eq!(
            app.inputs[0].value(),
            "typed",
            "re-entering the same workflow keeps input"
        );

        app.selected = 1;
        app.open_form();
        assert_eq!(
            app.inputs[0].value(),
            "",
            "switching workflows rebuilds the form"
        );
    }

    #[test]
    fn choice_dependencies_must_reference_earlier_fields() {
        let mut later = field_with("branch", true, None, Some("printf main"));
        later.depends_on = vec!["project".to_string()];
        let invalid_order = workflow(
            "invalid",
            "true",
            vec![later.clone(), field("project", true)],
        );
        assert!(validate_field_dependencies(&invalid_order)
            .unwrap_err()
            .contains("earlier field"));

        let mut no_command = field("branch", true);
        no_command.depends_on = vec!["project".to_string()];
        let invalid_kind = workflow(
            "invalid-kind",
            "true",
            vec![field("project", true), no_command],
        );
        assert!(validate_field_dependencies(&invalid_kind)
            .unwrap_err()
            .contains("no choices_command"));

        let valid = workflow("valid", "true", vec![field("project", true), later]);
        assert!(validate_field_dependencies(&valid).is_ok());
    }

    #[test]
    fn choice_environment_contains_only_declared_dependencies() {
        let mut branch = field_with("branch", true, None, Some("printf main"));
        branch.depends_on = vec!["project".to_string()];
        let mut wf = workflow("demo", "true", vec![field("project", true), branch.clone()]);
        wf.config
            .insert("root".to_string(), "/configured/root".to_string());
        let values = HashMap::from([
            ("project".to_string(), "alpha".to_string()),
            ("secret".to_string(), "not-for-choice-command".to_string()),
        ]);

        let environment = choice_environment(&wf, &branch, &values);
        assert_eq!(
            environment.get("HERDR_FIELD_project").map(String::as_str),
            Some("alpha")
        );
        assert!(!environment.contains_key("HERDR_FIELD_secret"));
        assert_eq!(
            environment
                .get("HERDR_WORKFLOW_CONFIG_ROOT")
                .map(String::as_str),
            Some("/configured/root")
        );
    }

    #[test]
    fn launcher_environment_filter_removes_inherited_field_values() {
        assert!(is_launcher_environment(OsStr::new("HERDR_FIELD_secret")));
        assert!(is_launcher_environment(OsStr::new(
            "HERDR_WORKFLOW_CONFIG_SECRET"
        )));
        assert!(!is_launcher_environment(OsStr::new("PATH")));
    }

    #[test]
    fn workflow_config_defaults_to_the_stable_name_namespace() {
        let config = LauncherConfig {
            workflows: HashMap::from([(
                "launch-agent".to_string(),
                HashMap::from([("profile".to_string(), "review".to_string())]),
            )]),
            ..LauncherConfig::default()
        };
        let mut workflow = workflow("launch-agent", "true", Vec::new());

        apply_workflow_config(&mut workflow, &config);

        assert_eq!(
            workflow_environment(&workflow, &HashMap::new())
                .get("HERDR_WORKFLOW_CONFIG_PROFILE")
                .map(String::as_str),
            Some("review")
        );
    }

    #[test]
    fn workflows_can_share_an_explicit_config_namespace() {
        let config = LauncherConfig {
            workflows: HashMap::from([(
                "worktrees".to_string(),
                HashMap::from([("project_root".to_string(), "/repos".to_string())]),
            )]),
            ..LauncherConfig::default()
        };
        let mut first = workflow("worktree-open", "true", Vec::new());
        first.config_namespace = Some("worktrees".to_string());
        let mut second = workflow("worktree-create", "true", Vec::new());
        second.config_namespace = Some("worktrees".to_string());

        apply_workflow_config(&mut first, &config);
        apply_workflow_config(&mut second, &config);

        assert_eq!(first.config, second.config);
        assert_eq!(
            first.config.get("project_root").map(String::as_str),
            Some("/repos")
        );
    }

    #[test]
    fn missing_workflow_namespace_exports_no_configuration() {
        let config = LauncherConfig::default();
        let mut workflow = workflow("missing", "true", Vec::new());

        apply_workflow_config(&mut workflow, &config);

        assert!(workflow.config.is_empty());
        assert!(!workflow_environment(&workflow, &HashMap::new())
            .keys()
            .any(|key| key.starts_with("HERDR_WORKFLOW_CONFIG_")));
    }

    #[test]
    fn empty_explicit_config_namespace_is_rejected() {
        let mut workflow = workflow("demo", "true", Vec::new());
        workflow.config_namespace = Some(String::new());
        assert!(validate_workflow(&workflow)
            .unwrap_err()
            .contains("config_namespace"));
    }

    #[test]
    fn invalid_workflow_configuration_is_a_main_config_error() {
        let root = env::temp_dir().join(format!("hl-config-validation-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let invalid_key = root.join("invalid-key.toml");
        fs::write(&invalid_key, "[workflows.demo]\nMixedCase = \"value\"\n").unwrap();
        assert!(load_config_from(Some(&invalid_key))
            .unwrap_err()
            .to_string()
            .contains("lowercase"));

        let collision = root.join("collision.toml");
        fs::write(
            &collision,
            "[workflows.demo]\nname = \"one\"\nNAME = \"two\"\n",
        )
        .unwrap();
        assert!(load_config_from(Some(&collision))
            .unwrap_err()
            .to_string()
            .contains("collide"));

        let non_string = root.join("non-string.toml");
        fs::write(&non_string, "[workflows.demo]\ncount = 2\n").unwrap();
        assert!(load_config_from(Some(&non_string)).is_err());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workflow_configuration_reaches_commands_and_choice_commands() {
        let mut workflow = workflow(
            "demo",
            "printf '%s' \"$HERDR_WORKFLOW_CONFIG_PROFILE\"",
            vec![field_with(
                "choice",
                true,
                None,
                Some("printf '%s\n' \"$HERDR_WORKFLOW_CONFIG_PROFILE\""),
            )],
        );
        workflow
            .config
            .insert("profile".to_string(), "review".to_string());

        let command_output = shell_command(
            &workflow.command,
            &workflow,
            workflow_environment(&workflow, &HashMap::new()),
        )
        .output()
        .unwrap();
        assert_eq!(String::from_utf8(command_output.stdout).unwrap(), "review");
        assert_eq!(
            run_choices(&workflow.fields[0], &workflow, &HashMap::new()).unwrap(),
            ["review"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn workflow_directory_environment_preserves_non_utf8_paths() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let mut workflow = workflow("demo", "true", Vec::new());
        workflow.dir =
            env::temp_dir().join(std::ffi::OsString::from_vec(vec![b'h', b'l', b'-', 0xff]));

        let output = shell_command(
            "printf '%s' \"$HERDR_WORKFLOW_DIR\"",
            &workflow,
            workflow_environment(&workflow, &HashMap::new()),
        )
        .output()
        .unwrap();

        assert_eq!(output.stdout, workflow.dir.as_os_str().as_bytes());
    }

    #[test]
    fn workflows_without_availability_checks_remain_available() {
        let mut workflow = workflow("demo", "true", Vec::new());

        evaluate_workflow_availability(&mut workflow);

        assert_eq!(workflow.availability, WorkflowAvailability::Available);
        assert!(ensure_workflow_available(&workflow).is_ok());
    }

    #[test]
    fn availability_checks_receive_namespaced_configuration() {
        let mut workflow = workflow("demo", "true", Vec::new());
        workflow.availability_command =
            Some("test \"$HERDR_WORKFLOW_CONFIG_REQUIRED_VALUE\" = configured".to_string());
        workflow
            .config
            .insert("required_value".to_string(), "configured".to_string());

        evaluate_workflow_availability(&mut workflow);

        assert_eq!(workflow.availability, WorkflowAvailability::Available);
    }

    #[test]
    fn availability_failures_use_sanitized_stderr_or_a_generic_fallback() {
        let mut explained = workflow("explained", "true", Vec::new());
        explained.availability_command = Some("printf 'Choose a valid path\\n' >&2; exit 1".into());
        evaluate_workflow_availability(&mut explained);
        assert_eq!(
            explained.availability,
            WorkflowAvailability::Disabled("Choose a valid path".to_string())
        );

        let mut silent = workflow("silent", "true", Vec::new());
        silent.availability_command = Some("exit 7".into());
        evaluate_workflow_availability(&mut silent);
        assert_eq!(
            silent.availability,
            WorkflowAvailability::Disabled("availability check failed (exit 7)".to_string())
        );
    }

    #[test]
    fn availability_spawn_failures_have_a_generic_reason() {
        let availability = availability_from_output(Err(io::Error::new(
            io::ErrorKind::NotFound,
            "shell missing",
        )));
        assert!(
            matches!(availability, WorkflowAvailability::Disabled(reason) if reason.contains("could not start"))
        );
    }

    #[test]
    fn disabled_workflows_render_a_reason_and_cannot_open() {
        let mut workflow = workflow("demo", "true", Vec::new());
        workflow.title = Some("Friendly Demo".to_string());
        workflow.availability =
            WorkflowAvailability::Disabled("configuration is missing".to_string());
        let mut app = App::new(vec![workflow]);

        let out = dump_snapshot(&mut app, 72, 8);
        assert!(out.contains("[disabled] Friendly Demo"), "{out}");
        assert!(out.contains("configuration is missing"), "{out}");
        assert!(!app.prepare_form());
        assert!(matches!(app.screen, Screen::List));
        assert!(app
            .error
            .as_deref()
            .is_some_and(|error| error.contains("configuration is missing")));
    }

    #[test]
    fn disabled_workflows_are_rejected_by_cli_guard() {
        let mut workflow = workflow("demo", "true", Vec::new());
        workflow.availability = WorkflowAvailability::Disabled("not ready".to_string());

        let error = ensure_workflow_available(&workflow).unwrap_err();

        assert!(error.contains("demo"));
        assert!(error.contains("not ready"));
    }

    #[test]
    fn choice_values_preserve_leading_and_trailing_spaces() {
        let field = field_with("project", true, None, Some("printf ' repo \nrepo\n'"));
        let wf = workflow("demo", "true", vec![field.clone()]);

        assert_eq!(
            run_choices(&field, &wf, &HashMap::new()).unwrap(),
            vec![" repo ".to_string(), "repo".to_string()]
        );
    }

    #[test]
    fn form_renders_generic_loading_feedback_before_choices_run() {
        let marker = env::temp_dir().join(format!("hl-loading-choices-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let command = format!("printf ran > '{}'; printf 'choice\n'", marker.display());
        let wf = workflow(
            "demo",
            "true",
            vec![field_with("project", true, None, Some(&command))],
        );
        let mut app = App::new(vec![wf]);

        assert!(app.prepare_form());
        let out = dump_snapshot(&mut app, 72, 14);
        assert!(out.contains("Loading choices"), "{out}");
        assert!(
            !marker.exists(),
            "choices must wait until feedback is drawn"
        );

        app.refresh_form_choices();
        assert!(marker.exists());
        assert_eq!(app.inputs[0].value(), "choice");
        let _ = fs::remove_file(marker);
    }

    #[test]
    fn dependent_choice_refreshes_before_submit_and_drops_stale_value() {
        let mut branch = field_with(
            "branch",
            true,
            None,
            Some("printf '%s-branch\n' \"$HERDR_FIELD_project\""),
        );
        branch.depends_on = vec!["project".to_string()];
        let wf = workflow(
            "demo",
            "true",
            vec![
                field_with("project", true, None, Some("printf 'alpha\nbeta\n'")),
                branch,
            ],
        );
        let mut app = App::new(vec![wf]);
        app.open_form();
        assert_eq!(app.inputs[1].value(), "alpha-branch");

        app.field_idx = 0;
        app.handle_current_input(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.inputs[0].value(), "beta");
        assert_eq!(app.inputs[1].value(), "alpha-branch");

        app.try_submit();
        let (_, values) = app.submit.expect("submit should refresh dependent choices");
        assert_eq!(values.get("branch"), Some(&"beta-branch".to_string()));
    }

    #[test]
    fn dependent_refresh_can_render_loading_before_running() {
        let marker = env::temp_dir().join(format!("hl-dependent-loading-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let mut branch = field_with(
            "branch",
            true,
            None,
            Some(&format!(
                "printf ran > '{}'; printf '%s-branch\\n' \"$HERDR_FIELD_project\"",
                marker.display()
            )),
        );
        branch.depends_on = vec!["project".to_string()];
        let workflow = workflow(
            "demo",
            "true",
            vec![
                field_with("project", true, None, Some("printf 'alpha\\nbeta\\n'")),
                branch,
            ],
        );
        let mut app = App::new(vec![workflow]);
        app.open_form();
        let _ = fs::remove_file(&marker);

        app.field_idx = 0;
        app.handle_current_input(KeyEvent::from(KeyCode::Down));
        app.move_field(1);
        assert!(app.choice_needs_refresh(app.field_idx));
        app.loading_choices = true;
        let out = dump_snapshot(&mut app, 72, 14);
        assert!(out.contains("Loading choices"), "{out}");
        assert!(!marker.exists(), "choice command ran before loading draw");

        app.refresh_current_choice();
        app.loading_choices = false;
        assert!(marker.exists());
        assert_eq!(app.inputs[1].value(), "beta-branch");
        let _ = fs::remove_file(marker);
    }

    #[test]
    fn unresolved_required_dependency_does_not_run_downstream_choices() {
        let marker =
            env::temp_dir().join(format!("hl-unresolved-dependency-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let mut branch = field_with(
            "branch",
            true,
            None,
            Some(&format!("printf ran > '{}'; exit 9", marker.display())),
        );
        branch.depends_on = vec!["project".to_string()];
        let wf = workflow(
            "demo",
            "true",
            vec![field_with("project", true, None, Some("true")), branch],
        );
        let mut app = App::new(vec![wf]);

        app.open_form();
        assert!(app.error.is_none());
        assert!(!marker.exists());

        app.try_submit();
        assert!(app.submit.is_none());
        assert!(app
            .error
            .as_deref()
            .is_some_and(|error| error.contains("project") && error.contains("required")));
        assert!(!marker.exists());
    }

    #[test]
    fn failed_choice_evaluation_is_visible_and_retryable() {
        let marker = env::temp_dir().join(format!("hl-choice-retry-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let command = format!(
            "test -f '{}' && printf 'ready\n' || {{ printf 'not ready' >&2; exit 1; }}",
            marker.display()
        );
        let wf = workflow(
            "demo",
            "true",
            vec![field_with("choice", true, None, Some(&command))],
        );
        let mut app = App::new(vec![wf]);
        app.open_form();
        assert!(app
            .error
            .as_deref()
            .is_some_and(|error| error.contains("not ready")));

        fs::write(&marker, "ready").unwrap();
        app.try_submit();
        assert!(app.submit.is_some());
        assert_eq!(app.inputs[0].value(), "ready");
        let _ = fs::remove_file(marker);
    }

    #[test]
    fn unchanged_dependency_signature_does_not_rerun_choices() {
        let counter = env::temp_dir().join(format!("hl-choice-count-{}", std::process::id()));
        let _ = fs::remove_file(&counter);
        let command = format!(
            "n=$(cat '{}' 2>/dev/null || printf 0); n=$((n + 1)); printf '%s' \"$n\" > '{}'; printf 'only\n'",
            counter.display(),
            counter.display()
        );
        let wf = workflow(
            "demo",
            "true",
            vec![field_with("choice", true, None, Some(&command))],
        );
        let mut app = App::new(vec![wf]);
        app.open_form();
        app.refresh_choice(0).unwrap();

        assert_eq!(fs::read_to_string(&counter).unwrap(), "1");
        let _ = fs::remove_file(counter);
    }

    #[test]
    fn reopening_form_refreshes_choices_without_dropping_typed_input() {
        let choices = env::temp_dir().join(format!("hl-reopen-choices-{}", std::process::id()));
        let _ = fs::remove_file(&choices);
        fs::write(&choices, "alpha\n").unwrap();
        let command = format!("cat '{}'", choices.display());
        let wf = workflow(
            "demo",
            "true",
            vec![
                field("note", false),
                field_with("project", true, None, Some(&command)),
            ],
        );
        let mut app = App::new(vec![wf]);
        app.open_form();
        app.inputs[0] = FieldInput::Line(Input::new("typed".to_string()));
        assert_eq!(app.inputs[1].value(), "alpha");

        fs::write(&choices, "beta\n").unwrap();
        app.screen = Screen::List;
        app.open_form();

        assert_eq!(app.inputs[0].value(), "typed");
        assert_eq!(app.inputs[1].value(), "beta");
        let _ = fs::remove_file(choices);
    }

    #[test]
    fn empty_and_failed_choice_sets_remain_constrained_in_cli() {
        let empty = workflow(
            "empty",
            "true",
            vec![field_with("choice", true, None, Some("true"))],
        );
        let supplied = HashMap::from([("choice".to_string(), "anything".to_string())]);
        let empty_error = collect_fields(&empty, &supplied, false, |_| None).unwrap_err();
        assert!(
            empty_error.contains("no choices available"),
            "{empty_error}"
        );

        let failed = workflow(
            "failed",
            "true",
            vec![field_with(
                "choice",
                true,
                None,
                Some("printf 'choice command broke' >&2; exit 9"),
            )],
        );
        let failed_error = collect_fields(&failed, &supplied, false, |_| None).unwrap_err();
        assert!(
            failed_error.contains("choice command broke"),
            "{failed_error}"
        );
    }

    #[test]
    fn empty_choice_set_cannot_restore_a_tui_default() {
        let wf = workflow(
            "empty",
            "true",
            vec![field_with("choice", true, Some("fallback"), Some("true"))],
        );
        let mut app = App::new(vec![wf]);

        app.open_form();
        app.try_submit();

        assert!(app.submit.is_none());
        assert_eq!(app.inputs[0].value(), "");
        assert!(app
            .error
            .as_deref()
            .is_some_and(|error| error.contains("required")));
    }

    #[test]
    fn cli_dependent_choices_use_the_selected_upstream_value() {
        let mut branch = field_with(
            "branch",
            true,
            None,
            Some("printf '%s-branch\n' \"$HERDR_FIELD_project\""),
        );
        branch.depends_on = vec!["project".to_string()];
        let wf = workflow(
            "demo",
            "true",
            vec![
                field_with("project", true, None, Some("printf 'alpha\nbeta\n'")),
                branch,
            ],
        );
        let valid = HashMap::from([
            ("project".to_string(), "beta".to_string()),
            ("branch".to_string(), "beta-branch".to_string()),
        ]);
        let invalid = HashMap::from([
            ("project".to_string(), "beta".to_string()),
            ("branch".to_string(), "alpha-branch".to_string()),
        ]);

        assert!(collect_fields(&wf, &valid, false, |_| None).is_ok());
        assert!(collect_fields(&wf, &invalid, false, |_| None)
            .unwrap_err()
            .contains("invalid value"));
    }

    #[test]
    fn config_path_prefers_the_herdr_plugin_directory() {
        let plugin = Path::new("/tmp/herdr-plugin-config");
        let home = Path::new("/tmp/home");
        assert_eq!(
            config_path(Some(plugin), Some(home)),
            Some(plugin.join("config.toml"))
        );
        assert_eq!(
            config_path(None, Some(home)),
            Some(home.join(".config/herdr-launcher/config.toml"))
        );
    }

    #[test]
    fn missing_config_defaults_to_split() {
        let path = env::temp_dir().join(format!("hl-missing-config-{}", std::process::id()));
        let config = load_config_from(Some(path.as_path())).unwrap();
        assert_eq!(config.placement, Placement::Split);
    }

    #[test]
    fn config_accepts_split_and_overlay() {
        let path = env::temp_dir().join(format!("hl-valid-config-{}", std::process::id()));

        fs::write(&path, "placement = \"split\"\n").unwrap();
        assert_eq!(
            load_config_from(Some(path.as_path())).unwrap().placement,
            Placement::Split
        );

        fs::write(&path, "placement = \"overlay\"\n").unwrap();
        assert_eq!(
            load_config_from(Some(path.as_path())).unwrap().placement,
            Placement::Overlay
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn unrelated_config_errors_do_not_include_placement_guidance() {
        let path =
            env::temp_dir().join(format!("hl-invalid-workflow-config-{}", std::process::id()));
        fs::write(&path, "[workflows.demo]\nproject_root = 42\n").unwrap();

        let error = load_config_from(Some(path.as_path()))
            .unwrap_err()
            .to_string();

        assert!(error.contains("project_root"), "{error}");
        assert!(!error.contains("placement"), "{error}");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn pane_open_args_use_placement_and_preserve_cwd_without_direction() {
        let args = pane_open_args("plugin.id", Placement::Overlay, Some("/repo"), None);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--placement", "overlay"]));
        assert!(args.windows(2).any(|pair| pair == ["--cwd", "/repo"]));
        assert!(!args.contains(&"--direction"));
    }

    #[test]
    fn invalid_config_uses_safe_split_and_visible_error_transport() {
        let path = env::temp_dir().join(format!("hl-invalid-config-{}", std::process::id()));
        fs::write(&path, "placement = \"popup\"\n").unwrap();
        let error = load_config_from(Some(path.as_path()))
            .unwrap_err()
            .to_string();
        let args = pane_open_args("plugin.id", Placement::Split, None, Some(&error));

        assert!(args.windows(2).any(|pair| pair == ["--placement", "split"]));
        assert!(args.windows(2).any(|pair| {
            pair[0] == "--env"
                && pair[1].starts_with("HERDR_LAUNCHER_CONFIG_ERROR=")
                && pair[1].contains("split")
                && pair[1].contains("overlay")
        }));
        assert!(!args.contains(&"--direction".to_string()));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn startup_error_snapshot_is_blocking_and_actionable() {
        let mut app = App::with_startup_error(
            "invalid Launcher config at /tmp/config.toml: placement must be split or overlay"
                .to_string(),
        );
        let out = dump_snapshot(&mut app, 80, 10);
        assert!(out.contains("Launcher configuration error"), "{out}");
        assert!(out.contains("/tmp/config.toml"), "{out}");
        assert!(out.contains("split or overlay"), "{out}");
        assert!(out.contains("Esc/q close"), "{out}");
    }
}
