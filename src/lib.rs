//! Pure logic for the herdr-title-rename plugin.
//!
//! Everything here is I/O-free so it can be unit tested without a running
//! Herdr server: the caller supplies a snapshot, the current state, and a
//! resolver that turns a directory into names, and gets back a plan of
//! rename calls plus the window title to set.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// How the working directory is rendered in the window title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathStyle {
    /// `$HOME`-relative with a leading tilde, matching tmux's
    /// `#{s|$HOME|~|:pane_current_path}`.
    Tilde,
    /// The absolute path, unmodified.
    Full,
    /// Only the final component.
    Basename,
    /// Omit the path entirely.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Joins the title fields. tmux used `" | "`.
    pub separator: String,
    pub path_style: PathStyle,
    /// Rename tabs after their active pane's directory.
    pub rename_tabs: bool,
    /// Rename workspaces after their active pane's repo (and branch).
    pub rename_workspaces: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            separator: " | ".to_string(),
            path_style: PathStyle::Tilde,
            rename_tabs: true,
            rename_workspaces: true,
        }
    }
}

impl Config {
    pub fn parse(raw: &str) -> Result<Self, String> {
        toml::from_str(raw).map_err(|error| format!("invalid config.toml: {error}"))
    }
}

// ---------------------------------------------------------------------------
// Snapshot (the subset of `session.snapshot` this plugin reads)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub number: Option<u32>,
    #[serde(default)]
    pub active_tab_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Tab {
    pub tab_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub number: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Pane {
    pub pane_id: String,
    pub tab_id: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub focused: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Layout {
    pub tab_id: String,
    #[serde(default)]
    pub focused_pane_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub tabs: Vec<Tab>,
    #[serde(default)]
    pub panes: Vec<Pane>,
    #[serde(default)]
    pub layouts: Vec<Layout>,
    #[serde(default)]
    pub focused_workspace_id: Option<String>,
    #[serde(default)]
    pub focused_tab_id: Option<String>,
    #[serde(default)]
    pub focused_pane_id: Option<String>,
}

/// Unwrap the `{"result": {"snapshot": {...}}}` envelope the socket returns.
pub fn snapshot_from_response(raw: &str) -> Result<Snapshot, String> {
    #[derive(Deserialize)]
    struct Envelope {
        result: Option<Body>,
        error: Option<serde_json::Value>,
    }
    #[derive(Deserialize)]
    struct Body {
        snapshot: Option<Snapshot>,
    }

    let envelope: Envelope =
        serde_json::from_str(raw).map_err(|error| format!("malformed API response: {error}"))?;
    if let Some(error) = envelope.error {
        return Err(format!("API error: {error}"));
    }
    envelope
        .result
        .and_then(|body| body.snapshot)
        .ok_or_else(|| "API response carried no snapshot".to_string())
}

impl Snapshot {
    /// The pane whose directory names a tab: the tab's focused pane per the
    /// layout, else any pane flagged focused, else the tab's first pane.
    /// tmux names a window after its *active* pane, and so do we.
    pub fn active_pane_of_tab(&self, tab_id: &str) -> Option<&Pane> {
        let from_layout = self
            .layouts
            .iter()
            .find(|layout| layout.tab_id == tab_id)
            .and_then(|layout| layout.focused_pane_id.as_deref())
            .and_then(|pane_id| self.panes.iter().find(|pane| pane.pane_id == pane_id));
        if from_layout.is_some() {
            return from_layout;
        }
        self.panes
            .iter()
            .find(|pane| pane.tab_id == tab_id && pane.focused)
            .or_else(|| self.panes.iter().find(|pane| pane.tab_id == tab_id))
    }

    fn active_pane_of_workspace(&self, workspace: &Workspace) -> Option<&Pane> {
        workspace
            .active_tab_id
            .as_deref()
            .and_then(|tab_id| self.active_pane_of_tab(tab_id))
    }
}

// ---------------------------------------------------------------------------
// Names derived from a directory
// ---------------------------------------------------------------------------

/// What a directory is called, in the two shapes the plugin needs. A port of
/// `tmux-window-name` (tab) and `tmux-session-name` (workspace).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Names {
    /// Worktree/repo root basename, else the directory basename.
    pub tab: String,
    /// `repo` on the default branch, `repo/branch` elsewhere, else basename.
    pub workspace: String,
}

// ---------------------------------------------------------------------------
// State — which labels this plugin owns
// ---------------------------------------------------------------------------

/// Labels this plugin last wrote, keyed by id. A label that no longer matches
/// what we wrote was renamed by hand, and we stop managing it — the analogue
/// of tmux turning `automatic-rename` off on a manual `rename-window`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub tabs: HashMap<String, String>,
    #[serde(default)]
    pub workspaces: HashMap<String, String>,
    /// Ids the user renamed by hand; never touched again.
    #[serde(default)]
    pub released: Vec<String>,
}

impl State {
    pub fn parse(raw: &str) -> Self {
        serde_json::from_str(raw).unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    fn is_released(&self, id: &str) -> bool {
        self.released.iter().any(|entry| entry == id)
    }

    fn release(&mut self, id: &str) {
        if !self.is_released(id) {
            self.released.push(id.to_string());
        }
    }
}

/// Herdr labels new tabs and workspaces with their number (`"1"`, `"2"`, …).
/// Those are placeholders, not user intent, so they are ours to overwrite.
fn is_default_label(label: &str, number: Option<u32>) -> bool {
    let label = label.trim();
    if label.is_empty() {
        return true;
    }
    match number {
        Some(number) => label == number.to_string(),
        None => label.chars().all(|character| character.is_ascii_digit()),
    }
}

/// Decide whether we may write `desired` over `current`.
///
/// Returns `None` when the label is already right or the user has taken it
/// over; `Some(desired)` when a rename should be issued.
fn plan_label<'a>(
    state: &mut State,
    kind: LabelKind,
    id: &str,
    current: &str,
    number: Option<u32>,
    desired: &'a str,
) -> Option<&'a str> {
    if desired.trim().is_empty() || state.is_released(id) {
        return None;
    }

    let ours = match kind {
        LabelKind::Tab => state.tabs.get(id),
        LabelKind::Workspace => state.workspaces.get(id),
    };

    match ours {
        // We have written this label before. If it still reads back as what we
        // wrote, it is ours to update; otherwise the user renamed it by hand.
        Some(previous) if previous == current => {}
        // …unless it reads back as a Herdr default. Nobody renames a tab *to*
        // its own number, so this is a rename of ours that has not landed yet
        // (concurrent events can observe the label mid-flight). Re-issue rather
        // than mistaking our own lag for the user's intent.
        Some(_) if is_default_label(current, number) => {}
        Some(_) => {
            state.release(id);
            return None;
        }
        // First time we have seen it: adopt it only if it is still a default.
        None if is_default_label(current, number) => {}
        None => {
            state.release(id);
            return None;
        }
    }

    if current == desired {
        // Record ownership even when no call is needed, so a later manual
        // rename is still detected.
        match kind {
            LabelKind::Tab => state.tabs.insert(id.to_string(), desired.to_string()),
            LabelKind::Workspace => state.workspaces.insert(id.to_string(), desired.to_string()),
        };
        return None;
    }

    match kind {
        LabelKind::Tab => state.tabs.insert(id.to_string(), desired.to_string()),
        LabelKind::Workspace => state.workspaces.insert(id.to_string(), desired.to_string()),
    };
    Some(desired)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LabelKind {
    Tab,
    Workspace,
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rename {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub tabs: Vec<Rename>,
    pub workspaces: Vec<Rename>,
    /// `None` means there is nothing focused to describe; clear the title.
    pub title: Option<String>,
}

/// Build the full plan: which tabs and workspaces to rename, and the window
/// title that follows from the labels those renames will leave behind.
///
/// `resolve` maps a directory to its names; the caller supplies the git-aware
/// implementation. `state` is updated in place to record what we now own.
pub fn plan(
    snapshot: &Snapshot,
    config: &Config,
    state: &mut State,
    home: Option<&str>,
    mut resolve: impl FnMut(&str) -> Names,
) -> Plan {
    let mut plan = Plan::default();
    // Labels as they will read *after* this plan is applied, so the title
    // never lags a rename by one event.
    let mut effective_tab: HashMap<&str, String> = HashMap::new();
    let mut effective_workspace: HashMap<&str, String> = HashMap::new();

    if config.rename_tabs {
        for tab in &snapshot.tabs {
            let Some(pane) = snapshot.active_pane_of_tab(&tab.tab_id) else {
                continue;
            };
            if pane.cwd.trim().is_empty() {
                continue;
            }
            let names = resolve(&pane.cwd);
            if let Some(label) = plan_label(
                state,
                LabelKind::Tab,
                &tab.tab_id,
                &tab.label,
                tab.number,
                &names.tab,
            ) {
                effective_tab.insert(tab.tab_id.as_str(), label.to_string());
                plan.tabs.push(Rename {
                    id: tab.tab_id.clone(),
                    label: label.to_string(),
                });
            }
        }
    }

    if config.rename_workspaces {
        for workspace in &snapshot.workspaces {
            let Some(pane) = snapshot.active_pane_of_workspace(workspace) else {
                continue;
            };
            if pane.cwd.trim().is_empty() {
                continue;
            }
            let names = resolve(&pane.cwd);
            if let Some(label) = plan_label(
                state,
                LabelKind::Workspace,
                &workspace.workspace_id,
                &workspace.label,
                workspace.number,
                &names.workspace,
            ) {
                effective_workspace.insert(workspace.workspace_id.as_str(), label.to_string());
                plan.workspaces.push(Rename {
                    id: workspace.workspace_id.clone(),
                    label: label.to_string(),
                });
            }
        }
    }

    plan.title = build_title(snapshot, config, home, &effective_tab, &effective_workspace);
    plan
}

fn build_title(
    snapshot: &Snapshot,
    config: &Config,
    home: Option<&str>,
    effective_tab: &HashMap<&str, String>,
    effective_workspace: &HashMap<&str, String>,
) -> Option<String> {
    let workspace_id = snapshot.focused_workspace_id.as_deref()?;
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == workspace_id);
    let tab_id = snapshot
        .focused_tab_id
        .as_deref()
        .or_else(|| workspace.and_then(|workspace| workspace.active_tab_id.as_deref()));
    let tab = tab_id.and_then(|tab_id| snapshot.tabs.iter().find(|tab| tab.tab_id == tab_id));

    let pane = snapshot
        .focused_pane_id
        .as_deref()
        .and_then(|pane_id| snapshot.panes.iter().find(|pane| pane.pane_id == pane_id))
        .or_else(|| tab_id.and_then(|tab_id| snapshot.active_pane_of_tab(tab_id)));

    let workspace_label = workspace.map(|workspace| {
        effective_workspace
            .get(workspace.workspace_id.as_str())
            .cloned()
            .unwrap_or_else(|| workspace.label.clone())
    });
    let tab_label = tab.map(|tab| {
        effective_tab
            .get(tab.tab_id.as_str())
            .cloned()
            .unwrap_or_else(|| tab.label.clone())
    });
    let path = pane.map(|pane| render_path(&pane.cwd, config.path_style, home));

    let fields: Vec<String> = [workspace_label, tab_label, path]
        .into_iter()
        .flatten()
        .map(|field| field.trim().to_string())
        .filter(|field| !field.is_empty())
        .collect();

    (!fields.is_empty()).then(|| fields.join(&config.separator))
}

/// Render a directory for the title. `Tilde` reproduces tmux's
/// `#{s|$HOME|~|:pane_current_path}`: `$HOME` itself becomes `~`, paths under
/// it get a `~/` prefix, everything else is left alone.
pub fn render_path(cwd: &str, style: PathStyle, home: Option<&str>) -> String {
    let cwd = cwd.trim();
    match style {
        PathStyle::None => String::new(),
        PathStyle::Full => cwd.to_string(),
        PathStyle::Basename => basename(cwd).to_string(),
        PathStyle::Tilde => match home.map(str::trim).filter(|home| !home.is_empty()) {
            Some(home) if cwd == home => "~".to_string(),
            Some(home) => match cwd.strip_prefix(home).filter(|rest| rest.starts_with('/')) {
                Some(rest) => format!("~{rest}"),
                None => cwd.to_string(),
            },
            None => cwd.to_string(),
        },
    }
}

pub fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return path;
    }
    match trimmed.rsplit_once('/') {
        Some((_, name)) if !name.is_empty() => name,
        _ => trimmed,
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

pub fn session_snapshot_request() -> String {
    serde_json::json!({
        "id": "herdr-title-rename:snapshot",
        "method": "session.snapshot",
        "params": {},
    })
    .to_string()
}

pub fn window_title_request(title: Option<&str>) -> String {
    match title {
        Some(title) => serde_json::json!({
            "id": "herdr-title-rename:title-set",
            "method": "client.window_title.set",
            "params": { "title": title },
        }),
        None => serde_json::json!({
            "id": "herdr-title-rename:title-clear",
            "method": "client.window_title.clear",
            "params": {},
        }),
    }
    .to_string()
}

pub fn tab_rename_request(tab_id: &str, label: &str) -> String {
    serde_json::json!({
        "id": "herdr-title-rename:tab-rename",
        "method": "tab.rename",
        "params": { "tab_id": tab_id, "label": label },
    })
    .to_string()
}

pub fn workspace_rename_request(workspace_id: &str, label: &str) -> String {
    serde_json::json!({
        "id": "herdr-title-rename:workspace-rename",
        "method": "workspace.rename",
        "params": { "workspace_id": workspace_id, "label": label },
    })
    .to_string()
}

/// Surface an `error` object from a response; `Ok(())` otherwise.
pub fn check_response(raw: &str) -> Result<(), String> {
    #[derive(Deserialize)]
    struct Envelope {
        error: Option<serde_json::Value>,
    }
    let envelope: Envelope =
        serde_json::from_str(raw).map_err(|error| format!("malformed API response: {error}"))?;
    match envelope.error {
        Some(error) => Err(format!("API error: {error}")),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/Users/adam";

    fn snapshot(json: serde_json::Value) -> Snapshot {
        serde_json::from_value(json).expect("snapshot")
    }

    /// One workspace, one tab, one focused pane — the common case.
    fn simple_snapshot(workspace_label: &str, tab_label: &str, cwd: &str) -> Snapshot {
        snapshot(serde_json::json!({
            "workspaces": [{
                "workspace_id": "w1", "label": workspace_label,
                "number": 1, "active_tab_id": "w1:t1",
            }],
            "tabs": [{"tab_id": "w1:t1", "workspace_id": "w1", "label": tab_label, "number": 1}],
            "panes": [{"pane_id": "w1:p1", "tab_id": "w1:t1", "cwd": cwd, "focused": true}],
            "layouts": [{"tab_id": "w1:t1", "focused_pane_id": "w1:p1"}],
            "focused_workspace_id": "w1",
            "focused_tab_id": "w1:t1",
            "focused_pane_id": "w1:p1",
        }))
    }

    fn names(tab: &str, workspace: &str) -> Names {
        Names {
            tab: tab.to_string(),
            workspace: workspace.to_string(),
        }
    }

    #[test]
    fn title_matches_the_tmux_three_field_shape() {
        let snapshot = simple_snapshot("chezmoi", "chezmoi", "/Users/adam/.local/share/chezmoi");
        let mut state = State::default();
        let plan = plan(
            &snapshot,
            &Config::default(),
            &mut state,
            Some(HOME),
            |_| names("chezmoi", "chezmoi"),
        );
        assert_eq!(
            plan.title.as_deref(),
            Some("chezmoi | chezmoi | ~/.local/share/chezmoi")
        );
    }

    #[test]
    fn default_numeric_labels_are_adopted_and_renamed() {
        let snapshot = simple_snapshot("1", "1", "/Users/adam/dev/moov/infra/feature");
        let mut state = State::default();
        let plan = plan(
            &snapshot,
            &Config::default(),
            &mut state,
            Some(HOME),
            |_| names("feature", "infra/feature"),
        );

        assert_eq!(
            plan.tabs,
            vec![Rename {
                id: "w1:t1".to_string(),
                label: "feature".to_string()
            }]
        );
        assert_eq!(
            plan.workspaces,
            vec![Rename {
                id: "w1".to_string(),
                label: "infra/feature".to_string()
            }]
        );
        // The title reflects the labels the renames will leave behind, not the
        // stale ones in the snapshot.
        assert_eq!(
            plan.title.as_deref(),
            Some("infra/feature | feature | ~/dev/moov/infra/feature")
        );
    }

    #[test]
    fn a_label_we_never_set_is_left_alone() {
        let snapshot = simple_snapshot("my-project", "notes", "/Users/adam/notes");
        let mut state = State::default();
        let plan = plan(
            &snapshot,
            &Config::default(),
            &mut state,
            Some(HOME),
            |_| names("notes-repo", "notes-repo"),
        );

        assert!(plan.tabs.is_empty());
        assert!(plan.workspaces.is_empty());
        assert!(state.released.contains(&"w1:t1".to_string()));
        assert_eq!(plan.title.as_deref(), Some("my-project | notes | ~/notes"));
    }

    #[test]
    fn a_manual_rename_after_we_took_ownership_releases_the_label() {
        let mut state = State::default();

        // First pass adopts the default label.
        let first = simple_snapshot("1", "1", "/Users/adam/dev/app");
        let plan_one = plan(&first, &Config::default(), &mut state, Some(HOME), |_| {
            names("app", "app")
        });
        assert_eq!(plan_one.tabs.len(), 1);

        // Second pass: the user has renamed the tab by hand.
        let second = simple_snapshot("app", "hand-picked", "/Users/adam/dev/app");
        let plan_two = plan(&second, &Config::default(), &mut state, Some(HOME), |_| {
            names("app", "app")
        });
        assert!(plan_two.tabs.is_empty());
        assert!(state.released.contains(&"w1:t1".to_string()));

        // And it stays released even after the directory changes.
        let third = simple_snapshot("app", "hand-picked", "/Users/adam/dev/other");
        let plan_three = plan(&third, &Config::default(), &mut state, Some(HOME), |_| {
            names("other", "other")
        });
        assert!(plan_three.tabs.is_empty());
    }

    #[test]
    fn a_rename_still_in_flight_is_not_mistaken_for_a_manual_one() {
        let mut state = State::default();

        // Adopt and rename.
        let first = simple_snapshot("1", "1", "/Users/adam/dev/app");
        assert_eq!(
            plan(&first, &Config::default(), &mut state, Some(HOME), |_| {
                names("app", "app")
            })
            .tabs
            .len(),
            1
        );

        // A concurrent event sees the label before the rename has landed. The
        // default label is proof it is still ours, not the user's doing.
        let racing = simple_snapshot("1", "1", "/Users/adam/dev/app");
        let plan_two = plan(&racing, &Config::default(), &mut state, Some(HOME), |_| {
            names("app", "app")
        });
        assert!(!state.released.contains(&"w1:t1".to_string()));
        assert_eq!(plan_two.tabs[0].label, "app");
    }

    #[test]
    fn an_unchanged_label_issues_no_call_but_keeps_ownership() {
        let mut state = State::default();
        let first = simple_snapshot("1", "1", "/Users/adam/dev/app");
        plan(&first, &Config::default(), &mut state, Some(HOME), |_| {
            names("app", "app")
        });

        let second = simple_snapshot("app", "app", "/Users/adam/dev/app");
        let plan_two = plan(&second, &Config::default(), &mut state, Some(HOME), |_| {
            names("app", "app")
        });
        assert!(plan_two.tabs.is_empty());
        assert!(!state.released.contains(&"w1:t1".to_string()));
        assert_eq!(state.tabs.get("w1:t1").map(String::as_str), Some("app"));
    }

    #[test]
    fn renaming_can_be_switched_off_without_losing_the_title() {
        let config = Config {
            rename_tabs: false,
            rename_workspaces: false,
            ..Config::default()
        };
        let snapshot = simple_snapshot("1", "1", "/Users/adam/dev/app");
        let mut state = State::default();
        let plan = plan(&snapshot, &config, &mut state, Some(HOME), |_| {
            names("app", "app")
        });

        assert!(plan.tabs.is_empty());
        assert!(plan.workspaces.is_empty());
        assert_eq!(plan.title.as_deref(), Some("1 | 1 | ~/dev/app"));
    }

    #[test]
    fn the_tab_is_named_after_its_active_pane_not_its_first() {
        let snapshot = snapshot(serde_json::json!({
            "workspaces": [{"workspace_id": "w1", "label": "1", "number": 1, "active_tab_id": "w1:t1"}],
            "tabs": [{"tab_id": "w1:t1", "workspace_id": "w1", "label": "1", "number": 1}],
            "panes": [
                {"pane_id": "w1:p1", "tab_id": "w1:t1", "cwd": "/Users/adam/dev/first"},
                {"pane_id": "w1:p2", "tab_id": "w1:t1", "cwd": "/Users/adam/dev/active"},
            ],
            "layouts": [{"tab_id": "w1:t1", "focused_pane_id": "w1:p2"}],
            "focused_workspace_id": "w1",
            "focused_tab_id": "w1:t1",
            "focused_pane_id": "w1:p2",
        }));
        let mut state = State::default();
        let plan = plan(
            &snapshot,
            &Config::default(),
            &mut state,
            Some(HOME),
            |cwd| names(basename(cwd), basename(cwd)),
        );
        assert_eq!(plan.tabs[0].label, "active");
    }

    #[test]
    fn an_empty_snapshot_clears_the_title() {
        let snapshot = snapshot(serde_json::json!({}));
        let mut state = State::default();
        let plan = plan(
            &snapshot,
            &Config::default(),
            &mut state,
            Some(HOME),
            |_| Names::default(),
        );
        assert_eq!(plan.title, None);
    }

    #[test]
    fn path_styles() {
        assert_eq!(
            render_path("/Users/adam/dev/app", PathStyle::Tilde, Some(HOME)),
            "~/dev/app"
        );
        assert_eq!(render_path(HOME, PathStyle::Tilde, Some(HOME)), "~");
        // A sibling that merely shares the prefix must not be rewritten.
        assert_eq!(
            render_path("/Users/adamant/dev", PathStyle::Tilde, Some(HOME)),
            "/Users/adamant/dev"
        );
        assert_eq!(
            render_path("/opt/homebrew", PathStyle::Tilde, Some(HOME)),
            "/opt/homebrew"
        );
        assert_eq!(
            render_path("/Users/adam/dev/app", PathStyle::Full, Some(HOME)),
            "/Users/adam/dev/app"
        );
        assert_eq!(
            render_path("/Users/adam/dev/app", PathStyle::Basename, Some(HOME)),
            "app"
        );
        assert_eq!(render_path("/Users/adam", PathStyle::None, Some(HOME)), "");
    }

    #[test]
    fn basenames() {
        assert_eq!(basename("/Users/adam/dev/app"), "app");
        assert_eq!(basename("/Users/adam/dev/app/"), "app");
        assert_eq!(basename("/"), "/");
        assert_eq!(basename("app"), "app");
    }

    #[test]
    fn config_defaults_reproduce_the_tmux_format() {
        let config = Config::parse("").expect("empty config");
        assert_eq!(config, Config::default());
        assert_eq!(config.separator, " | ");
        assert_eq!(config.path_style, PathStyle::Tilde);

        let config = Config::parse("path_style = \"basename\"\nrename_tabs = false\n")
            .expect("partial config");
        assert_eq!(config.path_style, PathStyle::Basename);
        assert!(!config.rename_tabs);
        assert!(config.rename_workspaces);

        assert!(Config::parse("nonsense = 1").is_err());
    }

    #[test]
    fn snapshot_envelope_handling() {
        let snapshot = snapshot_from_response(
            r#"{"id":"x","result":{"type":"session_snapshot","snapshot":{"workspaces":[{"workspace_id":"w1","label":"a"}]}}}"#,
        )
        .expect("snapshot");
        assert_eq!(snapshot.workspaces.len(), 1);

        assert!(snapshot_from_response(r#"{"error":{"code":"nope"}}"#).is_err());
        assert!(snapshot_from_response(r#"{"result":{}}"#).is_err());
        assert!(snapshot_from_response("not json").is_err());
    }

    #[test]
    fn state_round_trips_and_survives_garbage() {
        let mut state = State::default();
        state.tabs.insert("w1:t1".to_string(), "app".to_string());
        state.release("w1:t2");
        let reloaded = State::parse(&state.to_json());
        assert_eq!(reloaded.tabs.get("w1:t1").map(String::as_str), Some("app"));
        assert!(reloaded.is_released("w1:t2"));
        assert!(State::parse("").tabs.is_empty());
    }

    #[test]
    fn requests_carry_the_documented_methods() {
        assert!(session_snapshot_request().contains("\"session.snapshot\""));
        assert!(window_title_request(Some("t")).contains("\"client.window_title.set\""));
        assert!(window_title_request(None).contains("\"client.window_title.clear\""));
        assert!(tab_rename_request("w1:t1", "app").contains("\"tab.rename\""));
        assert!(workspace_rename_request("w1", "app").contains("\"workspace.rename\""));
        assert!(check_response(r#"{"result":{"type":"ok"}}"#).is_ok());
        assert!(check_response(r#"{"error":{"code":"tab_not_found"}}"#).is_err());
    }
}
