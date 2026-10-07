# Host-side jj/git helper, embedded in the plugin and run via `sh -c`.
# The plugin sandbox cannot touch repositories, so every VCS operation lives here.
#
# Usage: ws.sh DIR COMMAND [ARGS], run in DIR. The plugin starts it in `/`:
# zellij rewrites a command cwd under /tmp, /data, /cache or /host to plugin
# sandbox directories, arguments are passed through untouched.
#
#   info              print "<vcs>\t<main root>\t<workspace root>\t<workspace name>"
#                     for DIR; exit 3 when DIR is not in a jj or git repository
#   add VCS PATH NAME create workspace/worktree NAME at PATH (git: branch NAME)
#   remove VCS ROOT NAME
#                     jj: forget workspace NAME (its changes stay in the repo) and
#                     delete ROOT; git: `git worktree remove ROOT` (refuses if dirty)
set -eu

cd "$1"
cmd=$2
shift 2
case $cmd in
info)
  if root=$(jj workspace root --ignore-working-copy 2>/dev/null); then
    # The main checkout holds the repo store in .jj/repo; other workspaces
    # have a file there pointing at it (relative to their .jj). Not using
    # `jj workspace root --name default`: it fails when the main workspace
    # is renamed or its path was never recorded (repos from older jj).
    if [ -f "$root/.jj/repo" ]; then
      store=$(cat "$root/.jj/repo")
      case $store in /*) ;; *) store=$root/.jj/$store ;; esac
      main=$(cd "$store/../.." && pwd -P)
    else
      main=$root
    fi
    name=$(jj workspace list --ignore-working-copy -T 'name ++ "\t" ++ root ++ "\n"' |
      awk -F '\t' -v r="$root" '$2 == r { print $1; exit }')
    printf 'jj\t%s\t%s\t%s\n' "$main" "$root" "${name:-default}"
  elif root=$(git rev-parse --show-toplevel 2>/dev/null); then
    main=$(git worktree list --porcelain | sed -n '1s/^worktree //p')
    if [ "$root" = "$main" ]; then name=main; else name=$(basename "$root"); fi
    printf 'git\t%s\t%s\t%s\n' "$main" "$root" "$name"
  else
    exit 3
  fi
  ;;
add)
  vcs=$1 dir=$2 name=$3
  mkdir -p "$(dirname "$dir")"
  if [ "$vcs" = jj ]; then
    jj workspace add --name "$(basename "$dir")" "$dir"
  elif git show-ref --verify --quiet "refs/heads/$name"; then
    git worktree add "$dir" "$name"
  else
    git worktree add -b "$name" "$dir"
  fi
  ;;
remove)
  vcs=$1 root=$2 name=$3
  if [ "$vcs" = jj ]; then
    # forget runs from another workspace: snapshot first, or edits made in
    # this one since its last jj command are lost
    jj --repository "$root" util snapshot
    jj workspace forget "$name"
    rm -rf -- "$root"
  else
    git worktree remove "$root"
  fi
  ;;
*)
  echo "unknown command: $cmd" >&2
  exit 2
  ;;
esac
