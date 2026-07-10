# Changelog

All notable changes to herdr-launcher are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-07-10

### Added

- Field names are validated as POSIX shell identifiers at load time. A workflow with
  an unreferenceable name (e.g. `branch-name`, which would export an unusable
  `$HERDR_FIELD_branch-name`) is now skipped with a clear message instead of loading
  and silently failing at command time.
- Form input is preserved when leaving to the workflow list and re-entering the same
  workflow, so Esc-to-peek no longer discards everything typed. Selecting a different
  workflow still rebuilds the form.
- Unit test suite (15 tests) covering prompt/EOF resolution, defaults, choices
  validation, deterministic duplicate resolution, the form scrolling window,
  `SelectField` filtering, and form-state preservation.
- `launch-agent` example workflow: create a worktree and launch an agent on a prompt.

### Fixed

- Ctrl+D (EOF) on a required prompt no longer loops forever. `prompt` distinguishes a
  closed stream (`read_line` returning `Ok(0)`) from an empty line, and `collect`
  aborts with an error instead of re-asking indefinitely.
- The form no longer hides focusable fields in a short pane. Rendering uses a scrolling
  window that always contains the focused field, so Tab can never park the cursor on a
  field that isn't on screen; a `field N/M` counter appears when fields are off-screen.
- Duplicate workflow-name resolution is now deterministic. Directory entries are sorted
  before loading, so "first workflow with a name wins" no longer depends on filesystem
  iteration order.
- Submitting a form with an empty required field now surfaces a red error message
  instead of only silently moving the cursor.

## [0.2.0] - 2026-07-01

### Changed

- Complete Rust rewrite: a [ratatui](https://ratatui.rs) TUI with fuzzy-select fields
  (nucleo-matcher) and self-contained TOML workflow bundles (`<name>/` directories
  holding a `.toml` plus helper scripts). Field values are passed to commands via
  `$HERDR_FIELD_<name>` environment variables rather than string interpolation, so a
  value containing shell metacharacters cannot inject. Supersedes the earlier
  shell-based "forms" implementation. Tagged `v0.2.0-beta.1` then promoted to `v0.2.0`.

## [0.1.0] - 2026-06-29

### Added

- Initial release as the herdr "forms" plugin.

### Changed

- Renamed the plugin from "forms" to `herdr-launcher`.
- Renamed the core concept from "form" to "workflow".
- Moved user workflows to `~/.config/herdr-launcher/workflows/`.

[Unreleased]: https://github.com/arjenblokzijl/herdr-launcher/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/arjenblokzijl/herdr-launcher/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/arjenblokzijl/herdr-launcher/compare/b2c2ce3...v0.2.0
[0.1.0]: https://github.com/arjenblokzijl/herdr-launcher/commit/b2c2ce3
