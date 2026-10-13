# zellij-workspaces

A [Zellij](https://zellij.dev) sidebar that manages
[jj workspaces](https://jj-vcs.github.io/jj/latest/working-copy/#workspaces) and git
worktrees, one Zellij session per workspace, grouped by project. Each workspace has as
many tabs as you open in it; the tab bar on top shows the current workspace's tabs, the
sidebar every workspace, like [herdr](https://herdr.dev)'s spaces.

```
 ┌ tabs of proj:feat-x: agent │ logs │ server ────┐
 │ ▼ proj (2)     │                               │
 │     default    │                               │
 │   ● feat-x     │                               │
 │ ▼ other (1)    │                               │
 │     main       │                               │
```

Everything lives in the plugin: no shell hooks, scripts or keybindings in your config.

- **Sessions named after their workspace.** A workspace's session is named
  `project:workspace`: the project is the main checkout's directory name, the workspace
  is the jj workspace name, or for git the worktree directory (`main` for the main
  worktree). Project names are unique: the first repository seen with a directory name
  gets it, others with the same name get their parent directories' names in front
  (`~/code/alice/nixos` → `alice-nixos`). The names are kept in
  `~/.local/state/zellij-workspaces/projects` (`<name><TAB><main checkout>` per line;
  edit it to rename a project for new sessions). A session Zellij named itself
  (`adjective-noun`, e.g. a plain `zellij`) that starts inside a repository is renamed
  after its workspace, unless that name is taken. New tabs open in the session's
  directory, the workspace root.
- **Grouped, collapsible sidebar** of all running sessions on the machine. Sessions
  named `group:label` are shown under `group`; other sessions land in **General**.
  Selecting a session switches to it. Groups and sessions can be reordered, collapsed
  and renamed.
- **`Alt g`: go to a session or open a repository** with fuzzy search. Lists the running
  sessions and every jj/git repository found below `project_roots` (default: your home
  directory, 4 levels deep, skipping hidden directories and repositories inside
  repositories). Picking a repository switches to the session of its main checkout,
  starting it if needed; `Alt w` there opens its other workspaces.
- **`Alt w`: new or existing workspace** for the focused pane's repository. Lists the
  repository's workspaces on disk, the main checkout included (type to filter, `↑`/`↓`
  to pick, `Tab` to complete); picking one, or typing its exact name, switches to its
  session, starting it if needed. A new name creates the workspace next to the main
  checkout in `<project>.ws/<name>` (jj: `jj workspace add`, on top of bookmark `<name>`
  if it exists; git: `git worktree add`, new branch `<name>` unless it exists; `/` in the
  name becomes `-` in the directory) and switches to its new session.
- **`Alt W`: remove the focused pane's workspace** after a `y` confirmation and end its
  session. Removing the workspace you are in moves you to another session of the
  project (or any other one, or a new session in the main checkout). Work done in it is
  never lost:
  - jj: the workspace is snapshotted, bookmark `<name>` is set on its newest change
    that only this workspace has (if there is any), then the workspace is forgotten and
    its directory deleted. Creating a workspace with the same name later continues on
    that bookmark. An unrelated bookmark with that name is never moved; removal is
    refused instead.
  - git: `git worktree remove`, which refuses if the worktree has uncommitted or
    untracked changes; the branch stays.

  Files ignored by `.gitignore` are deleted with the directory. The main checkout is
  never removed.
- **Closing tabs works as without the sidebar.** When the last pane of a tab exits, the
  tab closes. When that was the session's last tab, the session ends and you move to
  another session as with `Alt W`; Zellij quits only with the last session. Ending a
  session keeps its workspace on disk (reopen it with `Alt w`).
- **Agent status icons** per tab and per workspace (red `◆` needs input, teal `✓` done,
  yellow spinner while working), also for workspaces running in the background, read
  from [omp](https://omp.sh)'s terminal titles with no setup, or set over `zellij pipe`,
  e.g. from Claude Code hooks.
- **`zellij-ws` CLI and Agent Skill** to create, rename and remove workspaces from
  scripts and coding agents, e.g. to spin off a task into its own workspace and session.

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

Add the sidebar and Zellij's tab bar to your layout.
[`layouts/workspaces.kdl`](layouts/workspaces.kdl) is a complete one; copy it to
`~/.config/zellij/layouts/` and set `default_layout "workspaces"` in `config.kdl`. New
tabs copy the layout, and so do the sessions the sidebar starts, so every tab gets the
sidebar. Use a plain base layout as in that file: with `default_tab_template` the first
tab ends up without a terminal pane.

Running sessions keep the plugin they loaded. After an update, reload it in each session
(new sessions load the new one anyway):

```sh
zellij action start-or-reload-plugin file:~/.config/zellij/plugins/zellij-workspaces.wasm
```

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

`RunCommands` runs jj/git, `find` to look for repositories and, to rename other
sessions, `zellij`; `Reconfigure` registers the keybindings in memory (your config file
is not written).

## Configuration

In the layout's `plugin` block (defaults shown):

```kdl
plugin location="file:~/.config/zellij/plugins/zellij-workspaces.wasm" {
    new_key "Alt w"        // "" disables
    close_key "Alt W"      // "" disables
    goto_key "Alt g"       // "" disables
    project_roots "~"      // where Alt g looks for repositories, ":"-separated
    project_depth "4"      // how many directories below a root a repository may be
    separator ":"          // project<separator>workspace
    waiting_icon "◆"       // rendered red
    completed_icon "✓"     // rendered teal
    spinner "⣾⣽⣻⢿⡿⣟⣯⣷"    // working animation, one width-1 char per frame
}
```

Pick spinner frames on the [preview page](docs/spinner.html).

## Sidebar keys and mouse

Focus the sidebar pane, then:

| Key | Action |
|---|---|
| `j`/`k`, arrows | move the selection |
| `Enter`/`Space` | switch to the session, or collapse/expand the group |
| `Shift+J`/`Shift+K`, `Shift+↓`/`Shift+↑` | move the group, or the session within its group |
| `r` | rename the group (re-prefixes its sessions) or the session's label |

Left click switches or toggles, the scroll wheel moves the selection. Moves only change
the sidebar's order; group order, session order and collapse state are shared by all
sessions and persist. Renaming a session does not rename its workspace (`zellij-ws
rename` does both).

## Command line and agents

The Nix package also installs `zellij-ws`, which does from a shell what `Alt w` and
`Alt W` do, using the same `ws.sh`. Run it inside a repository:

```sh
zellij-ws list                       # "<name>\t<root>" per workspace, main checkout excluded
zellij-ws status [NAME]              # per workspace "<name>\t<state>\t<root>", state from the
                                     # sidebar: working, needs-input, done, open or closed
zellij-ws new NAME [-- CMD...]       # create or reopen NAME, start its session in the
                                     # background running CMD (default: a shell); prints its root
zellij-ws rename [OLD] NEW           # rename OLD (default: the current workspace) and its session
zellij-ws close NAME                 # remove NAME like Alt W and end its session
```

- `new` and `close` behave like the dialogs, including keeping jj work under a bookmark
  and refusing dirty git worktrees. `close` refuses the workspace it is run from and the
  one whose session it runs in.
- `rename` for jj renames the workspace and a bookmark of the old name that marks only
  its own work; the directory stays, as jj cannot move a workspace. For git it moves the
  worktree to `<project>.ws/NEW` and renames its branch if that was the old name. The
  main checkout is never renamed.
- Sessions are only started, renamed and ended inside zellij. `zellij-ws` assumes the
  default `separator` (`:`) and needs `jj`/`git` and the session's `zellij` on `PATH`.

`share/skills/zellij-workspaces/SKILL.md` is an [Agent Skill](https://agentskills.io)
teaching coding agents to use `zellij-ws`: start tasks for you in their own workspaces,
optionally with an agent that gets each going (`zellij-ws new fix-x -- claude "…"`), report
their status, rename a workspace they take over, and close workspaces only once asked
to. Agents do their own implementation work with their own subagents, not in these
workspaces, and never type into the agents running there. Link it into
the agent's skill directory, e.g. `~/.claude/skills/zellij-workspaces`.

## Agent status

Tabs running an agent get an icon in the tab bar (` ⏳` waiting, ` ⚙` working, ` ✅`
done, part of the tab name), and their session gets one in the sidebar: red `◆` while
an agent waits for input, a yellow spinner while one works, and a teal `✓` once one
finished. `◆` and `✓` are never set on the tab you are looking at and are cleared when
you switch to it, but `◆` returns when you leave a tab whose agent is still waiting. In
a session no client is attached to, nobody looks at any tab, so its active tab gets
them too. A tab with several agents, and a session with several tabs, shows the most
urgent state: waiting, then working, then done.

**omp** needs no setup: with its default `tui.titleState`, omp titles its terminal
`π <state> <session title>`, where the state is `!` (needs input), `>` (idle) or a
spinner frame (working). The sidebar reads the titles of all panes once a second (title
changes cause no plugin event) and marks a tab done when its agent goes from working to
idle. Every session's own sidebar does this, also in sessions started in the background.

Other agents send `zellij pipe --name "zellij-workspaces::<signal>::<pane_id>"` with
`<signal>` one of `waiting`, `completed`, `working`, `clear-working`; the tab containing
the pane gets the status. `working` stays until `waiting`, `completed` or
`clear-working` arrives.

The status is stored as a suffix on the tab name (` ⏳`, ` ✅`, ` ⚙`), the only state all
plugin instances see alike, including the sidebars of other sessions, which read it
from the session list.

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
  prefixes to plugin sandbox directories in paths a plugin passes, so a workspace
  session the sidebar starts there opens in the wrong directory. `zellij-ws` and jj/git
  commands are unaffected.
- **One plugin instance per tab:** every instance receives every event of its session.
  Status changes are idempotent renames, so nothing runs twice.
- **The session list is up to a second old:** Zellij reads other sessions' state only
  when a plugin asks, which the sidebar on screen does once a second.

## How it works

- **Sessions** are listed with `get_session_list`, which also sends every plugin of the
  session a `SessionUpdate`, and switched to with `switch_session`; a new workspace
  session is started with `switch_session_with_cwd`, so it gets the default layout. To
  end its own session, the plugin first switches the client away, then kills the
  session half a second later; killing it first would end the client with it.
- **Session names:** a session with a generated name runs [`src/ws.sh`](src/ws.sh),
  embedded in the plugin, through `run_command` on its first pane's directory to ask
  jj/git which workspace it belongs to, then calls `rename_session`.
- **Agent titles** are read by every running sidebar instance for all tabs of its
  session: zellij starts the sidebar of a tab opened in the background only when it is
  first shown. The renames that set and clear status suffixes are idempotent, so
  instances never conflict.
- **The keybindings** are added with `reconfigure` as `LaunchPlugin` actions that start
  a floating instance of the same plugin with `mode "new"`, `"close"` or `"goto"`, and
  the sidebar's `separator` and project settings. Zellij starts such a plugin in the
  focused pane's directory, which tells the dialog which repository it is about. They
  are registered again whenever a client attaches, as they only reach the clients
  connected at the time.
- **The dialog instance** runs `ws.sh add`/`remove`, then switches to, starts or kills
  the workspace's session and closes itself. The go-to picker runs `ws.sh projects`
  (`find` below the roots) and, for the picked repository, `ws.sh info`.
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

Added in this fork: one session per workspace listed in the sidebar instead of tabs,
session naming after the workspace, the create/remove dialogs and their self-registered
keybindings, `ws.sh` with its jj/git tests, the `zellij-ws` CLI with its Agent Skill,
and the Nix flake. The cwd-pipe auto-grouping and the shell worktree helpers of
zellij-vtabs were replaced by these.

## License

MIT, see [LICENSE](LICENSE). The original copyright notice of zellij-vtabs is kept there.
