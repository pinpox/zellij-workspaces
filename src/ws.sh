# Host-side jj/git helper, embedded in the plugin and run via `sh -c`.
# The plugin sandbox cannot touch repositories, so every VCS operation lives here.
#
# Usage: ws.sh DIR COMMAND [ARGS], run in DIR. The plugin starts it in `/`:
# zellij rewrites a command cwd under /tmp, /data, /cache or /host to plugin
# sandbox directories, arguments are passed through untouched.
#
#   info              print "<vcs>\t<main root>\t<workspace root>\t<workspace name>"
#                     for DIR; exit 3 when DIR is not in a jj or git repository
#   list              print "<name>\t<root>" for each workspace/worktree of the
#                     repository except the main checkout, if its directory exists
#   add VCS PATH NAME create workspace/worktree NAME at PATH (git: branch NAME;
#                     jj: on top of bookmark NAME if it exists); succeeds without
#                     changes if PATH already is a workspace of this repository,
#                     so the plugin just opens it again
#   remove VCS ROOT NAME
#                     jj: point bookmark NAME at the work only this workspace has,
#                     forget it and delete ROOT; git: `git worktree remove ROOT`
#                     (refuses if dirty; the branch stays)
set -eu

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
    printf 'jj\t%s\t%s\t%s\n' "$main" "$root" "${name:-default}"
  elif root=$(git rev-parse --show-toplevel 2>/dev/null); then
    main=$(git worktree list --porcelain | sed -n '1s/^worktree //p')
    if [ "$root" = "$main" ]; then name=main; else name=$(basename "$root"); fi
    printf 'git\t%s\t%s\t%s\n' "$main" "$root" "$name"
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
list)
  main=$(info | cut -f2)
  if [ "$(info | cut -f1)" = jj ]; then
    jj workspace list --ignore-working-copy -T 'name ++ "\t" ++ root ++ "\n"'
  else
    git worktree list --porcelain | sed -n 's/^worktree //p' |
      while IFS= read -r root; do printf '%s\t%s\n' "$(basename "$root")" "$root"; done
  fi | while IFS="$(printf '\t')" read -r name root; do
    if [ -n "$root" ] && [ "$root" != "$main" ] && [ -d "$root" ]; then
      printf '%s\t%s\n' "$name" "$root"
    fi
  done
  ;;
add)
  vcs=$1 dir=$2 name=$3
  if [ -e "$dir" ]; then
    # reopening a workspace whose tab was closed
    here=$(info | cut -f2)
    if there=$(cd "$dir" && info) &&
      [ "$(echo "$there" | cut -f2)" = "$here" ] &&
      [ "$(echo "$there" | cut -f3)" = "$dir" ]; then
      exit 0
    fi
    echo "$dir already exists and is not a workspace of this repository" >&2
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
*)
  echo "unknown command: $cmd" >&2
  exit 2
  ;;
esac
