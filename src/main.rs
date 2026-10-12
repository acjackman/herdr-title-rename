//! Entry point: one short-lived run per Herdr event, plus a small watcher.
//!
//! Read the session snapshot, work out the renames and the window title, and
//! apply them. Herdr invokes the binary on each of the events declared in
//! `herdr-plugin.toml`, and a run costs one socket round trip plus a few `git`
//! calls.
//!
//! A manifest cannot subscribe to `pane.updated`, the only event a bare `cd`
//! raises, so each run also makes sure one `--watch` process is following it
//! over the socket. The watcher refreshes when a pane's directory changes and
//! otherwise sleeps on the stream.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime};

use herdr_title_rename::{
    basename, check_response, events_subscribe_request, pane_cwd_from_event, plan,
    plugin_enabled_from_response, plugin_list_request, session_snapshot_request,
    snapshot_from_response, tab_rename_request, window_title_request, workspace_rename_request,
    Config, CwdTracker, Names, State,
};

const API_TIMEOUT: Duration = Duration::from_secs(5);
/// A lock older than this is assumed to belong to a run that died.
const LOCK_STALE_AFTER: Duration = Duration::from_secs(5);
/// Settle time after a directory change: one refresh per burst of events.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(150);
/// Reconnect attempts, a second apart, before deciding the server is gone.
/// A restarted server's startup hook launches a fresh watcher.
const WATCH_RECONNECTS: u32 = 10;
/// The watcher log is truncated at launch once it passes this size.
const WATCH_LOG_LIMIT: u64 = 256 * 1024;

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

    if env::args().any(|argument| argument == "--watch") {
        return watch(&socket);
    }

    // `--dry-run` reports what the run would do and changes nothing — neither
    // the session nor the state file. Useful for checking naming rules against
    // a live session before handing the plugin the keys.
    let dry_run = env::args().any(|argument| argument == "--dry-run");
    let result = refresh(&socket, dry_run);

    if !dry_run {
        // Never fails the run: the title above is what matters right now.
        if let Err(error) = ensure_watcher() {
            eprintln!("watcher: {error}");
        }
    }
    result
}

fn refresh(socket: &Path, dry_run: bool) -> Result<(), String> {
    let config = load_config()?;
    let state_path = state_dir()?.join("state.json");
    let _lock = (!dry_run)
        .then(|| Lock::acquire(&state_path.with_extension("lock")))
        .flatten();

    let mut state = fs::read_to_string(&state_path)
        .map(|raw| State::parse(&raw))
        .unwrap_or_default();

    let snapshot = snapshot_from_response(&call(socket, &session_snapshot_request())?)?;
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
        if let Err(error) = call(socket, &tab_rename_request(&rename.id, &rename.label))
            .and_then(|response| check_response(&response))
        {
            failures.push(format!("tab {}: {error}", rename.id));
        }
    }
    for rename in &plan.workspaces {
        if let Err(error) = call(socket, &workspace_rename_request(&rename.id, &rename.label))
            .and_then(|response| check_response(&response))
        {
            failures.push(format!("workspace {}: {error}", rename.id));
        }
    }

    let title = plan.title.as_deref();
    let title_result =
        call(socket, &window_title_request(title)).and_then(|response| check_response(&response));

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
// Watcher — a bare `cd` only reaches us over the socket
// ---------------------------------------------------------------------------

enum Stop {
    /// The server closed the stream (restart, live handoff): reconnect.
    Closed,
    /// The plugin was disabled or uninstalled: exit.
    Disabled,
    /// The binary was rebuilt under us (reinstall): re-exec the new one.
    Upgraded,
}

/// Start the watcher unless one is already running. Probing its lock decides;
/// the watcher takes the same lock itself, so a lost race only means the
/// second one exits straight away.
fn ensure_watcher() -> Result<(), String> {
    let directory = state_dir()?;
    fs::create_dir_all(&directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    if open_lock(&directory.join("watcher.lock"))?
        .try_lock()
        .is_err()
    {
        return Ok(());
    }

    let log_path = directory.join("watcher.log");
    let oversized = fs::metadata(&log_path)
        .map(|metadata| metadata.len() > WATCH_LOG_LIMIT)
        .unwrap_or(false);
    let mut options = fs::OpenOptions::new();
    if oversized {
        options.write(true).truncate(true);
    } else {
        options.append(true);
    }
    let log = options
        .create(true)
        .open(&log_path)
        .map_err(|error| format!("cannot open {}: {error}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .map_err(|error| format!("cannot open {}: {error}", log_path.display()))?;

    let exe = env::current_exe().map_err(|error| format!("cannot locate binary: {error}"))?;
    Command::new(exe)
        .arg("--watch")
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err)
        // Its own process group, so it is not torn down with this run.
        .process_group(0)
        .spawn()
        .map_err(|error| format!("cannot start: {error}"))?;
    Ok(())
}

fn watch(socket: &Path) -> Result<(), String> {
    let directory = state_dir()?;
    fs::create_dir_all(&directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    let lock = open_lock(&directory.join("watcher.lock"))?;
    if lock.try_lock().is_err() {
        return Ok(());
    }

    let exe = env::current_exe().map_err(|error| format!("cannot locate binary: {error}"))?;
    let built = modified(&exe);
    eprintln!("watching {}", socket.display());

    let mut failures = 0;
    loop {
        match follow(socket, &exe, built) {
            Ok(Stop::Closed) => failures = 0,
            Ok(Stop::Disabled) => {
                eprintln!("plugin disabled; stopping");
                return Ok(());
            }
            Ok(Stop::Upgraded) => {
                eprintln!("binary changed; restarting");
                drop(lock);
                // `exec` only returns on failure.
                let error = Command::new(&exe).arg("--watch").exec();
                return Err(format!("cannot restart: {error}"));
            }
            Err(error) => {
                failures += 1;
                if failures >= WATCH_RECONNECTS {
                    return Err(format!("giving up: {error}"));
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Follow one subscription until it ends, refreshing on directory changes.
fn follow(socket: &Path, exe: &Path, built: Option<SystemTime>) -> Result<Stop, String> {
    let stream = UnixStream::connect(socket)
        .map_err(|error| format!("cannot connect to {}: {error}", socket.display()))?;
    stream
        .set_read_timeout(Some(API_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(API_TIMEOUT)))
        .map_err(|error| format!("cannot configure socket: {error}"))?;
    let mut writer = &stream;
    writeln!(writer, "{}", events_subscribe_request())
        .map_err(|error| format!("cannot subscribe: {error}"))?;

    let mut reader = BufReader::new(&stream);
    let mut line = Vec::new();
    reader
        .read_until(b'\n', &mut line)
        .map_err(|error| format!("cannot subscribe: {error}"))?;
    check_response(&String::from_utf8_lossy(&line))?;
    line.clear();

    // Seed after subscribing, so no change can fall between the two.
    let snapshot = snapshot_from_response(&call(socket, &session_snapshot_request())?)?;
    let mut tracker = CwdTracker::seed(&snapshot);
    // Catch up on whatever changed while nobody was listening: a server
    // restart, or the change that made the previous binary re-exec.
    if let Err(error) = refresh(socket, false) {
        eprintln!("refresh: {error}");
    }

    // From here the read timeout is a tick for the debounce, not a deadline.
    stream
        .set_read_timeout(Some(WATCH_DEBOUNCE))
        .map_err(|error| format!("cannot configure socket: {error}"))?;
    let mut due: Option<Instant> = None;
    loop {
        // A timeout can land mid-line; `read_until` keeps what it has read in
        // `line`, so it is cleared only once a whole event is handled.
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => return Ok(Stop::Closed),
            Ok(_) => {
                if let Some(pane) = pane_cwd_from_event(&String::from_utf8_lossy(&line)) {
                    if tracker.observe(&pane.pane_id, &pane.cwd) {
                        due.get_or_insert_with(|| Instant::now() + WATCH_DEBOUNCE);
                    }
                }
                line.clear();
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(error) => return Err(format!("event stream: {error}")),
        }

        if due.is_some_and(|due| Instant::now() >= due) {
            due = None;
            if !plugin_enabled_from_response(&call(socket, &plugin_list_request())?)? {
                return Ok(Stop::Disabled);
            }
            if modified(exe) != built {
                return Ok(Stop::Upgraded);
            }
            if let Err(error) = refresh(socket, false) {
                eprintln!("refresh: {error}");
            }
        }
    }
}

fn open_lock(path: &Path) -> Result<fs::File, String> {
    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
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
