//! Entry point: one short-lived run per Herdr event.
//!
//! Read the session snapshot, work out the renames and the window title, and
//! apply them. There is no daemon — Herdr invokes the binary on each of the
//! events declared in `herdr-plugin.toml`, and a run costs one socket round
//! trip plus a few `git` calls.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, SystemTime};

use herdr_title_rename::{
    basename, check_response, plan, session_snapshot_request, snapshot_from_response,
    tab_rename_request, window_title_request, workspace_rename_request, Config, Names, State,
};

const API_TIMEOUT: Duration = Duration::from_secs(5);
/// A lock older than this is assumed to belong to a run that died.
const LOCK_STALE_AFTER: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let socket = socket_path()?;

    if env::args().any(|argument| argument == "--clear") {
        let response = call(&socket, &window_title_request(None))?;
        return check_response(&response);
    }

    // `--dry-run` reports what the run would do and changes nothing — neither
    // the session nor the state file. Useful for checking naming rules against
    // a live session before handing the plugin the keys.
    let dry_run = env::args().any(|argument| argument == "--dry-run");

    let config = load_config()?;
    let state_path = state_dir()?.join("state.json");
    let _lock = (!dry_run)
        .then(|| Lock::acquire(&state_path.with_extension("lock")))
        .flatten();

    let mut state = fs::read_to_string(&state_path)
        .map(|raw| State::parse(&raw))
        .unwrap_or_default();

    let snapshot = snapshot_from_response(&call(&socket, &session_snapshot_request())?)?;
    let home = env::var("HOME").ok();

    // `plan` asks for the same directory more than once (a tab and its
    // workspace share a pane); resolve each one only once per run.
    let mut resolved: HashMap<String, Names> = HashMap::new();
    let plan = plan(&snapshot, &config, &mut state, home.as_deref(), |cwd| {
        resolved
            .entry(cwd.to_string())
            .or_insert_with(|| resolve_names(cwd))
            .clone()
    });

    if dry_run {
        for rename in &plan.tabs {
            println!("would rename tab {} -> {}", rename.id, rename.label);
        }
        for rename in &plan.workspaces {
            println!("would rename workspace {} -> {}", rename.id, rename.label);
        }
        println!("title: {}", plan.title.as_deref().unwrap_or("(cleared)"));
        return Ok(());
    }

    // Persist before applying: if a rename call fails we would rather skip it
    // than re-issue it on every subsequent event.
    if let Some(parent) = state_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(&state_path, state.to_json())
        .map_err(|error| format!("cannot write {}: {error}", state_path.display()))?;

    // A failed rename is reported but must not stop the title from updating —
    // the title is the part time tracking depends on.
    let mut failures = Vec::new();
    for rename in &plan.tabs {
        if let Err(error) = call(&socket, &tab_rename_request(&rename.id, &rename.label))
            .and_then(|response| check_response(&response))
        {
            failures.push(format!("tab {}: {error}", rename.id));
        }
    }
    for rename in &plan.workspaces {
        if let Err(error) = call(
            &socket,
            &workspace_rename_request(&rename.id, &rename.label),
        )
        .and_then(|response| check_response(&response))
        {
            failures.push(format!("workspace {}: {error}", rename.id));
        }
    }

    let title = plan.title.as_deref();
    let title_result =
        call(&socket, &window_title_request(title)).and_then(|response| check_response(&response));

    // Herdr records stdout per plugin run, so this line is the log.
    println!("{}", title.unwrap_or("(cleared)"));

    match (title_result, failures.is_empty()) {
        (Err(error), _) => Err(error),
        (Ok(()), true) => Ok(()),
        (Ok(()), false) => Err(failures.join("; ")),
    }
}

// ---------------------------------------------------------------------------
// Naming — a port of tmux-window-name / tmux-session-name
// ---------------------------------------------------------------------------

fn resolve_names(cwd: &str) -> Names {
    Names {
        tab: tab_name(cwd),
        workspace: workspace_name(cwd),
    }
}

/// Worktree root basename, else the directory basename.
fn tab_name(cwd: &str) -> String {
    match git(cwd, &["rev-parse", "--show-toplevel"]) {
        Some(root) if !root.is_empty() => basename(&root).to_string(),
        _ => basename(cwd).to_string(),
    }
}

/// `repo` on the default branch, `repo/branch` on any other, else basename.
fn workspace_name(cwd: &str) -> String {
    // worktrunk keeps the bare repo in `.bare`; git commands belong in the
    // project root above it.
    let directory = if basename(cwd) == ".bare" {
        parent(cwd).unwrap_or(cwd)
    } else {
        cwd
    };

    if git(directory, &["rev-parse", "--git-dir"]).is_none() {
        return basename(directory).to_string();
    }
    if git(directory, &["rev-parse", "--is-bare-repository"]).as_deref() == Some("true") {
        return basename(directory).to_string();
    }

    let repo = repo_name(directory);
    let branch = git(directory, &["branch", "--show-current"]).unwrap_or_default();
    if branch.is_empty() {
        return repo;
    }

    // worktrunk knows the repo's default branch; without it, fall back to the
    // usual names so a plain clone still collapses to just the repo name.
    let default_branch = command(directory, "wt", &["config", "state", "default-branch"])
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "main".to_string());

    if branch == default_branch {
        repo
    } else {
        format!("{repo}/{branch}")
    }
}

/// The repo's name: the basename of its main worktree, or of the directory
/// holding `.bare` when worktrunk laid it out that way.
fn repo_name(directory: &str) -> String {
    let main_worktree = git(directory, &["worktree", "list", "--porcelain"])
        .and_then(|output| {
            output
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("worktree "))
                .map(str::to_string)
        })
        .unwrap_or_default();

    if main_worktree.is_empty() {
        return basename(directory).to_string();
    }
    if basename(&main_worktree) == ".bare" {
        return parent(&main_worktree)
            .map(|path| basename(path).to_string())
            .unwrap_or_else(|| basename(&main_worktree).to_string());
    }
    basename(&main_worktree).to_string()
}

fn parent(path: &str) -> Option<&str> {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit_once('/').map(|(head, _)| match head {
        "" => "/",
        head => head,
    })
}

fn git(directory: &str, arguments: &[&str]) -> Option<String> {
    command(directory, "git", arguments)
}

fn command(directory: &str, program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(arguments)
        .current_dir(directory)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// ---------------------------------------------------------------------------
// Socket
// ---------------------------------------------------------------------------

fn call(socket: &Path, request: &str) -> Result<String, String> {
    let stream = UnixStream::connect(socket)
        .map_err(|error| format!("cannot connect to {}: {error}", socket.display()))?;
    stream
        .set_read_timeout(Some(API_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(API_TIMEOUT)))
        .map_err(|error| format!("cannot configure socket: {error}"))?;

    let mut writer = &stream;
    writeln!(writer, "{request}").map_err(|error| format!("cannot send request: {error}"))?;
    writer
        .flush()
        .map_err(|error| format!("cannot flush request: {error}"))?;

    let mut response = String::new();
    BufReader::new(&stream)
        .read_line(&mut response)
        .map_err(|error| format!("cannot read response: {error}"))?;
    if response.trim().is_empty() {
        return Err("empty response from Herdr".to_string());
    }
    Ok(response)
}

fn socket_path() -> Result<PathBuf, String> {
    env::var_os("HERDR_SOCKET_PATH")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "HERDR_SOCKET_PATH is not set (run this as a Herdr plugin)".to_string())
}

fn plugin_dir(variable: &str) -> Result<PathBuf, String> {
    env::var_os(variable)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| format!("{variable} is not set (run this as a Herdr plugin)"))
}

fn state_dir() -> Result<PathBuf, String> {
    plugin_dir("HERDR_PLUGIN_STATE_DIR")
}

fn load_config() -> Result<Config, String> {
    let path = match plugin_dir("HERDR_PLUGIN_CONFIG_DIR") {
        Ok(directory) => directory.join("config.toml"),
        // No config directory is not an error: the defaults are the point.
        Err(_) => return Ok(Config::default()),
    };
    match fs::read_to_string(&path) {
        Ok(raw) => Config::parse(&raw),
        Err(_) => Ok(Config::default()),
    }
}

// ---------------------------------------------------------------------------
// Lock — events can fire concurrently, and they share one state file
// ---------------------------------------------------------------------------

struct Lock {
    path: PathBuf,
}

impl Lock {
    /// Best effort: serialise concurrent runs, but never block an event for
    /// long. A lock left behind by a killed run is broken once it goes stale.
    fn acquire(path: &Path) -> Option<Self> {
        for _ in 0..50 {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(_) => {
                    return Some(Self {
                        path: path.to_path_buf(),
                    })
                }
                Err(_) => {
                    if is_stale(path) {
                        let _ = fs::remove_file(path);
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        None
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn is_stale(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .and_then(|modified| {
            SystemTime::now()
                .duration_since(modified)
                .map_err(|_| std::io::Error::other("clock went backwards"))
        })
        .map(|age| age > LOCK_STALE_AFTER)
        .unwrap_or(false)
}
