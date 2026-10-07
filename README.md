# zellij-workspaces

A [Zellij](https://zellij.dev) sidebar that groups tabs by project and manages
[jj workspaces](https://jj-vcs.github.io/jj/latest/working-copy/#workspaces) and
git worktrees, one tab per workspace.

```
 ▼ proj (2)
     default
   ● feat-x
 ▼ other (1)
     main
```

Everything lives in the plugin: no shell hooks, scripts or keybindings in your config.

- **Tabs named after their workspace.** When a shell changes directory, the tab is
  renamed `project:workspace`: the project is the main checkout's directory name, the
  workspace is the jj workspace name, or for git the worktree directory (`main` for
  the main worktree). Tabs whose shell starts in a repository are named on creation.
  Directories outside a repository leave the name alone.
- **Grouped, collapsible sidebar.** `group:label` tabs are shown under `group`; other
  tabs land in **General**. Groups and tabs can be reordered, collapsed and renamed.
- **`Alt w`: new workspace** for the focused pane's repository. Asks for a name,
  creates it next to the main checkout in `<project>.ws/<name>` (jj: `jj workspace
  add`; git: `git worktree add`, new branch `<name>` unless it exists, `/` in the name
  becomes `-` in the directory) and opens it in a new tab.
- **`Alt W`: remove the focused tab's workspace** after a `y` confirmation and close the
  tab. jj: the workspace is snapshotted and forgotten, its changes stay in the repo, the
  directory is deleted. git: `git worktree remove`, which refuses if the worktree has
  changes. The main checkout is never removed.
- **Agent status icons** (`◆` needs input, `✓` done, spinner while working), set over
  `zellij pipe`, e.g. from Claude Code hooks.

Built on [zellij-vtabs](https://github.com/otezz/zellij-vtabs) by Seto Kuslaksono, see
[Credits](#credits).

## Requirements

- Zellij 0.44.2 or newer (the plugin is built against `zellij-tile` 0.44.2; builds
  against an older `zellij-tile` load on newer Zellij, not the other way round)
- `sh` and `jj` and/or `git` on the `PATH` of the Zellij server

## Install

Load the plugin from a fixed path such as `~/.config/zellij/plugins/zellij-workspaces.wasm`.
Zellij remembers granted permissions by that path (symlinks are not resolved), so a
fixed path keeps the grant across updates.

With Nix, build it and link the result to that path, e.g. with home-manager's
`xdg.configFile."zellij/plugins/zellij-workspaces.wasm".source`:

```sh
nix build github:pinpox/zellij-workspaces
# result/share/zellij/plugins/zellij-workspaces.wasm
```

From source (needs the `wasm32-wasip1` Rust target; `nix develop` provides it):

```sh
cargo build --release --target wasm32-wasip1
cp target/wasm32-wasip1/release/zellij-workspaces.wasm ~/.config/zellij/plugins/
```

Add the sidebar to your layout. [`layouts/workspaces.kdl`](layouts/workspaces.kdl) is a
complete one; copy it to `~/.config/zellij/layouts/` and set
`default_layout "workspaces"` in `config.kdl`. New tabs copy the layout, so every tab
gets the sidebar. Use a plain base layout as in that file: with `default_tab_template`
the first tab ends up without a terminal pane.

### Permissions

On first start the sidebar asks for `ReadApplicationState`, `ChangeApplicationState`,
`ReadCliPipes`, `RunCommands` and `Reconfigure`. Focus the sidebar (`Alt h` from the
pane next to it) and press `y`. Zellij stores the grant in
`~/.cache/zellij/permissions.kdl`; writing that entry yourself works too:

```kdl
"/home/you/.config/zellij/plugins/zellij-workspaces.wasm" {
    ReadApplicationState
    ChangeApplicationState
    ReadCliPipes
    RunCommands
    Reconfigure
}
```

`RunCommands` runs jj/git; `Reconfigure` registers the two keybindings in memory (your
config file is not written).

## Configuration

In the layout's `plugin` block (defaults shown):

```kdl
plugin location="file:~/.config/zellij/plugins/zellij-workspaces.wasm" {
    new_key "Alt w"        // "" disables
    close_key "Alt W"      // "" disables
    separator ":"          // project<separator>workspace
    waiting_icon "◆"       // rendered yellow
    completed_icon "✓"     // rendered green
    spinner "⣾⣽⣻⢿⡿⣟⣯⣷"    // working animation, one width-1 char per frame
}
```

Pick spinner frames on the [preview page](docs/spinner.html).

## Sidebar keys and mouse

Focus the sidebar pane, then:

| Key | Action |
|---|---|
| `j`/`k`, arrows | move the selection |
| `Enter`/`Space` | switch to the tab, or collapse/expand the group |
| `Shift+J`/`Shift+K`, `Shift+↓`/`Shift+↑` | move the group, or the tab within its group |
| `r` | rename the group (re-prefixes its tabs) or the tab's label |

Left click switches or toggles, the scroll wheel moves the selection. Tab moves only
change the sidebar's order; group order, tab order and collapse state persist per
session.

## Agent status

Send `zellij pipe --name "zellij-workspaces::<signal>::<pane_id>"` with `<signal>` one of
`waiting`, `completed`, `working`, `clear-working`; the tab containing the pane gets the
status. `waiting`/`completed` are never set on the tab you are looking at and are
cleared when you switch to it; `working` stays until `waiting`, `completed` or
`clear-working` arrives.

The status is stored as a suffix on the tab name (` ⏳`, ` ✅`, ` ⚙`), the only state all
per-tab plugin instances see alike. Workspace renames keep it.

[`shell/agent-status.sh`](shell/agent-status.sh) drives this from Claude Code hooks,
counting subagents so the spinner stays on until the whole task is done, and plays a
freedesktop sound when a task finishes or needs input:

```json
{
  "hooks": {
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "timeout": 3,
      "command": "(~/.config/zellij/agent-status.sh prompt >/dev/null 2>&1 &)" }] }],
    "SubagentStart": [{ "hooks": [{ "type": "command", "timeout": 3,
      "command": "(~/.config/zellij/agent-status.sh subagent-start >/dev/null 2>&1 &)" }] }],
    "SubagentStop": [{ "hooks": [{ "type": "command", "timeout": 3,
      "command": "(~/.config/zellij/agent-status.sh subagent-stop >/dev/null 2>&1 &)" }] }],
    "Notification": [{ "hooks": [{ "type": "command", "timeout": 3,
      "command": "(~/.config/zellij/agent-status.sh notify >/dev/null 2>&1 &)" }] }],
    "Stop": [{ "hooks": [{ "type": "command", "timeout": 3,
      "command": "(~/.config/zellij/agent-status.sh stop >/dev/null 2>&1 &)" }] }],
    "SessionEnd": [{ "hooks": [{ "type": "command", "timeout": 3,
      "command": "(~/.config/zellij/agent-status.sh end >/dev/null 2>&1 &)" }] }]
  }
}
```

The script pipes with `< /dev/null` (otherwise `zellij pipe` reads the hook's stdin and
hangs) and `timeout 3` (no listening plugin, e.g. a session without this layout).

## Limitations

- **Repositories under `/tmp`, `/data`, `/cache` or `/host`:** Zellij maps these
  prefixes to plugin sandbox directories when a plugin opens a tab, so a new workspace
  tab there opens in the wrong directory. jj/git commands are unaffected.
- **Manual tab names are overwritten** on the next directory change inside a repository.
- **One plugin instance per tab:** every instance receives every event. Only the
  instance in a pane's own tab reacts to its directory changes, and status changes are
  idempotent renames, so nothing runs twice.

## How it works

- **Sidebar instances** react to `CwdChanged` and probe new default-named tabs with
  `get_pane_cwd`. They run [`src/ws.sh`](src/ws.sh), embedded in the plugin, through
  `run_command` to ask jj/git which workspace a directory belongs to, then rename the
  tab with `rename_tab_with_id`.
- **The keybindings** are added with `reconfigure` as `LaunchPlugin` actions that start
  a floating instance of the same plugin with `mode "new"` or `mode "close"`. Zellij
  starts such a plugin in the focused pane's directory, which tells the dialog which
  repository it is about.
- **The dialog instance** runs `ws.sh add`/`remove`, then calls `new_tab` or
  `close_tab_with_id` and closes itself.
- **`ws.sh` runs from `/`** with the repository directory as an argument, because Zellij
  rewrites a command cwd under `/tmp` etc. (see Limitations).

## Development

```sh
nix develop            # rust with wasm32-wasip1, jj, git
cargo test             # unit tests + ws.sh against real jj/git repos
cargo clippy --release --target wasm32-wasip1 -- -D warnings
cargo build --release --target wasm32-wasip1
zellij action start-or-reload-plugin file:target/wasm32-wasip1/release/zellij-workspaces.wasm
```

`nix build` runs the same tests.

## Credits

This is a fork of [otezz/zellij-vtabs](https://github.com/otezz/zellij-vtabs) by Seto
Kuslaksono, and most of the plugin is that work. From zellij-vtabs:

- the vertical sidebar: grouping tabs by `group:label`, collapsing, reordering, inline
  renaming, mouse and keyboard navigation, overflow-safe rendering
- per-session persistence of group order, tab order and collapse state shared across
  the per-tab plugin instances
- agent status icons encoded in tab names, their pipe interface, the spinner and its
  preview page, and the Claude Code hook script (`shell/agent-status.sh`, formerly
  `vtabs-work.sh`)
- the design notes on why state has to live in tab names and the cache directory

Added in this fork: workspace-based tab naming from cwd events, the create/remove
dialogs and their self-registered keybindings, `ws.sh` with its jj/git tests, and the
Nix flake. The cwd-pipe auto-grouping and the shell worktree helpers of zellij-vtabs
were replaced by these.

## License

MIT, see [LICENSE](LICENSE). The original copyright notice of zellij-vtabs is kept there.
