# herdr-title-rename

A [Herdr](https://herdr.dev) plugin that reproduces tmux's window titling and
automatic window naming:

```
workspace | tab | ~/path
```

```text
chezmoi | chezmoi | ~/.local/share/chezmoi
infra/infra-4977-exempt-bots | infra-4977-exempt-bots | ~/dev/moov/infra/infra-4977-exempt-bots
```

Two halves, which is why the name has two words:

- **Title** — the focused workspace label, tab label, and the focused pane's
  working directory go into the outer terminal window title, via Herdr's
  `client.window_title.set` API. This is the half that matters for tools that
  attribute time or context from the terminal window title, such as
  [Timing](https://timingapp.com): keep the path in the title and a project
  stays identifiable.
- **Rename** — tabs are named after their active pane's git worktree (or plain
  directory), workspaces after `repo` or `repo/branch`. Rename anything by hand
  and the plugin leaves it alone from then on, the way tmux stops managing a
  window you `rename-window`.

The equivalent tmux configuration, for reference:

```tmux
set-titles-string '#S | #W | #{s|$HOME|~|:pane_current_path}'
```

## Requirements

- Herdr 0.7.5 or newer (the plugin uses `client.window_title.set`,
  `session.snapshot`, `tab.rename`, `workspace.rename`)
- macOS or Linux
- `git` on `PATH`
- A terminal that shows the OSC window title (Herdr emits it; no
  terminal-specific integration is involved)
- Rust 1.89+ to build — the plugin builds from source on install

Optional: [worktrunk](https://github.com/dhth/worktrunk) (`wt`). When present,
the repo's real default branch comes from `wt config state default-branch`;
otherwise `main` is assumed, which only matters for deciding when to drop the
`/branch` suffix.

## Install

```sh
herdr plugin install acjackman/herdr-title-rename
```

Local development:

```sh
git clone https://github.com/acjackman/herdr-title-rename
cd herdr-title-rename
cargo build --release --locked
herdr plugin link "$PWD"
```

Only one plugin can own the window title. If another title plugin is installed
(for example `rjyo.window-title-sync`), disable it first, or the two will
overwrite each other:

```sh
herdr plugin disable rjyo.window-title-sync
```

## Naming rules

| Directory | Tab | Workspace |
|---|---|---|
| Git worktree on the default branch | worktree basename | `repo` |
| Git worktree on another branch | worktree basename | `repo/branch` |
| Bare repo checkout (`.bare` layout) | directory basename | project directory name |
| Not a git repo | directory basename | directory basename |

A tab is named after its **active** pane, matching tmux's behaviour of naming a
window after the pane you are looking at.

Manual renames win permanently. The plugin records every label it writes; if a
label reads back as something else, you changed it, and that tab or workspace is
released for the rest of the session. A label the plugin has never owned is only
adopted if it is still Herdr's default (the tab or workspace number).

## Configuration

```sh
herdr plugin config-dir acjackman.title-rename
```

Create `config.toml` there — every key is optional and the defaults reproduce
the tmux format above:

```toml
separator = " | "     # joins the title fields
path_style = "tilde"  # tilde | full | basename | none
rename_tabs = true
rename_workspaces = true
```

- `path_style = "tilde"` mirrors tmux's `#{s|$HOME|~|:pane_current_path}`.
- Set `rename_tabs` and `rename_workspaces` to `false` to keep only the title
  sync.

## Actions

```sh
herdr plugin action invoke acjackman.title-rename.refresh
herdr plugin action invoke acjackman.title-rename.clear
```

## How it works

Herdr runs the binary on each subscribed event; there is no daemon and no
polling. A run is one `session.snapshot` request, a few `git` calls, and at most
three writes.

### Known limitation: a bare `cd` does not refresh

Herdr's plugin manifest accepts a **narrower** set of events than its socket
API. `pane.updated` — the event that carries a pane's directory and terminal
title changes — is valid for `events.subscribe` over the socket but is rejected
in a manifest, with `unknown event 'pane.updated'` logged at install. Verified
against Herdr 0.7.5 by linking a probe manifest; also rejected are
`layout.updated`, `pane.cwd_changed`, `pane.output_changed`, `pane.renamed`,
`pane.scroll_changed`, `pane.title_changed`, and `workspace.metadata_updated`.

Consequence: the title and tab names refresh when focus or session structure
changes, but not when you `cd` inside a pane you are already looking at. Switch
panes, tabs, or workspaces and it catches up; or run the `refresh` action.

The fix is a long-lived watcher holding a socket subscription to `pane.updated`,
started from `[[startup]]` — planned, not implemented.

State (which labels the plugin owns) lives in `HERDR_PLUGIN_STATE_DIR`, guarded
by a lock file so concurrent events do not clobber each other.

## Development

```sh
cargo test
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
```

The logic in `src/lib.rs` is I/O-free and unit tested against fixture
snapshots; `src/main.rs` holds the socket, git, and filesystem work.

## Prior art

- [rjyo/herdr-window-title-sync](https://github.com/rjyo/herdr-window-title-sync)
  — syncs the title from the agent session instead of the directory
- [filoozom/herdr-title](https://github.com/filoozom/herdr-title) — worktree
  name plus agent-activity spinner; the Rust plugin this one is modelled on

## License

MIT
