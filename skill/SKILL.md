---
name: zellij-workspaces
description: Set up separate tasks for the user in their own jj workspace or git worktree with their own zellij tab, optionally started by another agent, check how the agents in those workspaces are doing, and rename or remove workspaces via the `zellij-ws` CLI. Use when asked to prepare or start tasks (e.g. one per issue) for the user to work on in parallel, to report the status of workspaces or their agents, to open, list, rename or close a workspace or worktree, or to take over an existing workspace.
---

# Workspaces via `zellij-ws`

The user runs zellij with the zellij-workspaces sidebar: every jj workspace or git
worktree gets its own tab named `<project>:<workspace>`, grouped by project. `zellij-ws`
does from the command line what the sidebar's `Alt w` / `Alt W` dialogs do. Run it from
inside the repository; it operates on the repository of the current directory.

```bash
zellij-ws list                       # "<name>\t<root>" per workspace, main checkout excluded
zellij-ws status [NAME]              # "<name>\t<state>\t<root>" per workspace (or just NAME)
zellij-ws new NAME                   # create (or reopen) NAME, open it in a background tab with a shell
zellij-ws new NAME -- CMD [ARGS...]  # same, but the tab runs CMD instead of a shell
zellij-ws rename NEW                 # rename the workspace you are in, and its tab
zellij-ws rename OLD NEW             # rename workspace OLD, and its tab
zellij-ws close NAME                 # remove workspace NAME and close its tab
```

`new` and `rename` print the workspace root on stdout; progress and errors go to stderr.
A non-zero exit means nothing was done; read the message instead of retrying blindly.

## Where workspaces live

- New workspaces go next to the main checkout: `<parent>/<project>.ws/<name>`. A `/` in
  the name (branch style, `user/fix-x`) becomes `-` in the workspace and tab name.
- jj: `jj workspace add` on top of bookmark `NAME` if it exists, otherwise on top of the
  current working copy's parent. git: `git worktree add`, branch `NAME` (created unless it
  exists).
- Tabs open in the background (`--no-focus`): the user is not pulled away from what they
  are looking at. Tabs are only opened, renamed and closed inside zellij (`$ZELLIJ` set);
  outside it `new`, `rename` and `close` still manage the workspace on disk, and
  `new NAME -- CMD` is refused.

## Starting tasks for the user

Workspaces are for tasks the user works on, e.g. one per issue from a list you were
given. Start one only when a task needs its own working copy, and keep tasks you were
asked to do yourself out of them: do your own implementation work with your own
subagents, in your own working copy.

1. Pick a short, specific name per task (`fix-login-timeout`, `issue-123`, not `task2`).
2. Create it, usually with an agent that gets the task going. Its prompt must be
   self-contained; the new agent has none of your context:
   ```bash
   zellij-ws new fix-login-timeout -- omp "Fix the login timeout in src/auth.rs: ..."
   ```
   Use the agent CLI the user works with (`omp`, `claude`, ...). Without `-- CMD` the tab
   gets a plain shell.
3. Tell the user each tab name (`<project>:fix-login-timeout`) and what runs there. From
   then on the user works with that agent; do not type into its pane, steer it, or edit
   the workspace's files.

`new` with the name of an existing workspace reopens it instead of creating one; if its
tab is already open, it only reports that, and refuses to start a CMD in it.

## Reading status

`zellij-ws status` reports per workspace what the sidebar shows for its agent:

| state         | meaning                                                              |
| ------------- | -------------------------------------------------------------------- |
| `working`     | the agent is running                                                 |
| `needs-input` | the agent waits for an answer or approval                            |
| `done`        | the agent finished its turn and the user has not looked at the tab   |
| `open`        | tab open, nothing to report (idle and already seen, or no agent)     |
| `closed`      | no tab; the workspace exists on disk only                            |

`done` only means the agent stopped, not that the task is solved: check the result
itself (commits, a PR, CI, a deploy) before calling a task finished. Status needs
zellij. Report it when asked; do not loop on it waiting for agents to finish.

## Renaming and taking over

- `zellij-ws rename NEW` renames the workspace your current directory is in, e.g. when
  you continue the work in a workspace someone else started and its name no longer fits.
  `zellij-ws rename OLD NEW` renames another one. The main checkout is never renamed.
- jj: the workspace name changes, its directory stays where it is (jj cannot move a
  workspace), and a bookmark `OLD` that marks only this workspace's own work (left by an
  earlier close) is renamed along. git: the worktree directory moves to `<project>.ws/NEW`
  and its branch is renamed if it was `OLD`; shells inside it keep working.
- Renaming onto the name of an existing workspace or git branch is refused.

## Closing

Closing is for the user, or for you after the user agreed (e.g. once the work is
merged or explicitly abandoned):

- Only close workspaces you created or were told to close, and only when nothing runs in
  them any more: closing the tab kills whatever runs there.
- Never close the workspace you are in (refused) or the main checkout (never listed).
- jj: the workspace is snapshotted, bookmark `NAME` is set on its newest change that no
  other workspace has, then the workspace is forgotten and its directory deleted. The
  work stays in the repo; `zellij-ws new NAME` later continues on that bookmark.
- git: `git worktree remove`, refused while the worktree has uncommitted or untracked
  changes. Commit or ask the user; do not force it. The branch stays.
- Files ignored by `.gitignore` are deleted with the directory.
