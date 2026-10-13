# Host-side jj/git helper, embedded in the plugin and run via `sh -c`.
# The plugin sandbox cannot touch repositories, so every VCS operation lives here.
#
# Usage: ws.sh DIR COMMAND [ARGS], run in DIR. The plugin starts it in `/`:
# zellij rewrites a command cwd under /tmp, /data, /cache or /host to plugin
# sandbox directories, arguments are passed through untouched.
#
#   info              print "<vcs>\t<main root>\t<workspace root>\t<workspace name>
#                     \t<project name>" for DIR (see project_name); exit 3 when DIR
#                     is not in a jj or git repository
#   list [all]        print "<name>\t<root>" for each workspace/worktree of the
#                     repository if its directory exists; the main checkout
#                     only with `all` (named as by `info`)
#   add VCS PATH NAME create workspace/worktree NAME at PATH (git: branch NAME;
#                     jj: on top of bookmark NAME if it exists); succeeds without
#                     changes if PATH already is that workspace of this
#                     repository, so the plugin just opens it again
#   remove VCS ROOT NAME
#                     jj: point bookmark NAME at the work only this workspace has,
#                     forget it and delete ROOT; git: `git worktree remove ROOT`
#                     (refuses if dirty; the branch stays)
#   rename VCS ROOT OLD NEW
#                     rename workspace OLD at ROOT to NEW (`/` becomes `-`) and
#                     print its root afterwards. jj: the directory stays (jj
#                     cannot move a workspace), a bookmark OLD marking only this
#                     workspace's work is renamed along; git: the worktree moves
#                     next to ROOT, its branch is renamed if it is OLD
#   projects DEPTH ROOT...
#                     print "<path below ROOT>\t<path>" for each jj or git
#                     repository at most DEPTH directories below a ROOT (`~`
#                     is expanded), skipping hidden directories, workspaces in
#                     `*.ws` directories and repositories inside repositories
set -eu

# Project name of main checkout $1: its directory name, or for a second
# repository with that name, the name prefixed with its parent directories'
# names until it is unique ("owner-repo"). Names are kept in a registry the
# first time a repository is seen, so they never change afterwards.
project_name() {
  reg=${XDG_STATE_HOME:-$HOME/.local/state}/zellij-workspaces/projects
  name=$(awk -F '\t' -v r="$1" '$2 == r { print $1; exit }' "$reg" 2>/dev/null || true)
  if [ -z "$name" ]; then
    taken() { awk -F '\t' -v n="$1" '$1 == n { f = 1 } END { exit !f }' "$reg" 2>/dev/null; }
    name=$(basename "$1")
    parent=$(dirname "$1")
    n=1
    while taken "$name"; do
      if [ "$parent" != / ]; then
        name=$(basename "$parent")-$name
        parent=$(dirname "$parent")
      else
        n=$((n + 1))
        name=$(basename "$1")-$n
      fi
    done
    mkdir -p "$(dirname "$reg")"
    printf '%s\t%s\n' "$name" "$1" >>"$reg"
  fi
  printf %s "$name"
}

info() {
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
    printf 'jj\t%s\t%s\t%s\t%s\n' "$main" "$root" "${name:-default}" "$(project_name "$main")"
  elif root=$(git rev-parse --show-toplevel 2>/dev/null); then
    main=$(git worktree list --porcelain | sed -n '1s/^worktree //p')
    if [ "$root" = "$main" ]; then name=main; else name=$(basename "$root"); fi
    printf 'git\t%s\t%s\t%s\t%s\n' "$main" "$root" "$name" "$(project_name "$main")"
  else
    return 3
  fi
}

# quote a string for a jj revset string literal
revset_str() {
  printf '"%s"' "$(printf %s "$1" | sed 's/\\/\\\\/g; s/"/\\"/g')"
}

cd "$1"
cmd=$2
shift 2
case $cmd in
info)
  info
  ;;
projects)
  depth=$1
  shift
  for root in "$@"; do
    case $root in
    "~") root=$HOME ;;
    "~/"*) root=$HOME/${root#"~/"} ;;
    esac
    root=${root%/}
    [ -d "$root" ] || continue
    find "$root" -mindepth 1 -maxdepth $((depth + 1)) \
      \( -type d -name '*.ws' -prune \) -o \
      \( -type d \( -name .git -o -name .jj \) -print -prune \) -o \
      \( -type d -name '.*' -prune \) 2>/dev/null |
      sed 's|/[^/]*$||' | LC_ALL=C sort -u |
      awk -v r="$root/" '
        { for (k in kept) if (index($0, k "/") == 1) next
          kept[$0] = 1
          print (index($0, r) == 1 ? substr($0, length(r) + 1) : $0) "\t" $0 }'
  done
  ;;
list)
  main=$(info | cut -f2)
  vcs=$(info | cut -f1)
  all=${1:-}
  if [ "$vcs" = jj ]; then
    jj workspace list --ignore-working-copy -T 'name ++ "\t" ++ root ++ "\n"'
  else
    git worktree list --porcelain | sed -n 's/^worktree //p' |
      while IFS= read -r root; do printf '%s\t%s\n' "$(basename "$root")" "$root"; done
  fi | while IFS="$(printf '\t')" read -r name root; do
    if [ "$root" = "$main" ]; then
      [ "$all" = all ] || continue
      [ "$vcs" = jj ] || name=main
    fi
    if [ -n "$root" ] && [ -d "$root" ]; then
      printf '%s\t%s\n' "$name" "$root"
    fi
  done
  ;;
add)
  vcs=$1 dir=$2 name=$3
  if [ -e "$dir" ]; then
    # reopening a workspace whose session was ended
    here=$(info | cut -f2)
    if there=$(cd "$dir" && info) &&
      [ "$(echo "$there" | cut -f2)" = "$here" ] &&
      [ "$(echo "$there" | cut -f3)" = "$dir" ] &&
      [ "$(echo "$there" | cut -f4)" = "$(basename "$dir")" ]; then
      exit 0
    fi
    # also a jj workspace renamed away from its directory's name
    echo "$dir already exists and is not workspace $(basename "$dir") of this repository" >&2
    exit 1
  fi
  mkdir -p "$(dirname "$dir")"
  if [ "$vcs" = jj ]; then
    slug=$(basename "$dir")
    # continue the work a removed workspace of this name left behind
    bookmark="bookmarks(exact:$(revset_str "$slug"))"
    if [ -n "$(jj log --ignore-working-copy --no-graph -r "$bookmark" -T 'commit_id')" ]; then
      jj workspace add --name "$slug" -r "$bookmark" "$dir"
    else
      jj workspace add --name "$slug" "$dir"
    fi
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
    # keep the work only this workspace has reachable by name, like a branch
    ws="$(revset_str "$name")@"
    own="::$ws ~ ::(working_copies() ~ $ws)"
    work="($own) ~ empty()"
    if [ -n "$(jj log --ignore-working-copy --no-graph -r "$work" -T 'commit_id')" ]; then
      # only move a same-named bookmark if it already marks this workspace's
      # own work (from an earlier remove/reopen), never e.g. one on trunk
      bookmark="bookmarks(exact:$(revset_str "$name"))"
      if [ -n "$(jj log --ignore-working-copy --no-graph -r "$bookmark ~ ($own)" -T 'commit_id')" ]; then
        echo "bookmark $name points outside this workspace; not removing it, so its work keeps a name" >&2
        exit 1
      fi
      jj bookmark set "$name" --allow-backwards -r "latest(heads($work))"
    fi
    jj workspace forget "$name"
    rm -rf -- "$root"
  else
    git worktree remove "$root"
  fi
  ;;
rename)
  vcs=$1 root=$2 old=$3 new=$4
  slug=$(printf %s "$new" | tr / -)
  if [ "$vcs" = jj ]; then
    if jj workspace list --ignore-working-copy -T 'name ++ "\n"' | grep -qxF -- "$slug"; then
      echo "workspace $slug already exists" >&2
      exit 1
    fi
    # a bookmark left by remove/reopen keeps marking this workspace's work
    ws="$(revset_str "$old")@"
    own="::$ws ~ ::(working_copies() ~ $ws)"
    bookmark="bookmarks(exact:$(revset_str "$old"))"
    if [ -n "$(jj log --ignore-working-copy --no-graph -r "$bookmark" -T 'commit_id')" ] &&
      [ -z "$(jj log --ignore-working-copy --no-graph -r "$bookmark ~ ($own)" -T 'commit_id')" ]; then
      jj bookmark rename "$old" "$slug"
    fi
    jj --repository "$root" workspace rename "$slug"
    printf '%s\n' "$root"
  else
    dir=$(dirname "$root")/$slug
    if [ -e "$dir" ]; then
      echo "$dir already exists" >&2
      exit 1
    fi
    branch=$(git -C "$root" branch --show-current)
    rename_branch=
    if [ -n "$branch" ] && [ "$(printf %s "$branch" | tr / -)" = "$old" ]; then
      if git show-ref --verify --quiet "refs/heads/$new"; then
        echo "branch $new already exists" >&2
        exit 1
      fi
      rename_branch=1
    fi
    git worktree move "$root" "$dir"
    if [ -n "$rename_branch" ]; then
      git branch -m "$branch" "$new"
    fi
    printf '%s\n' "$dir"
  fi
  ;;
*)
  echo "unknown command: $cmd" >&2
  exit 2
  ;;
esac
