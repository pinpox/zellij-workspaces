//! Runs the embedded `ws.sh` against real jj and git repositories, the way
//! the plugin does (`sh -c <script> ws.sh <dir> <args>` from `/`). Needs `jj` and `git`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use super::WS_SH;

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let dir = std::env::temp_dir().join(format!("ws-sh-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("home")).unwrap();
        let dir = dir.canonicalize().unwrap();
        fs::write(
            dir.join("jj.toml"),
            "user.name = \"t\"\nuser.email = \"t@example.com\"\n",
        )
        .unwrap();
        Sandbox { dir }
    }

    fn cmd(&self, program: &str, cwd: &Path) -> Command {
        let mut c = Command::new(program);
        c.current_dir(cwd)
            .env("HOME", self.dir.join("home"))
            .env("JJ_CONFIG", self.dir.join("jj.toml"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com");
        c
    }

    fn run(&self, program: &str, args: &[&str], cwd: &Path) -> Output {
        let out = self.cmd(program, cwd).args(args).output().unwrap();
        assert!(out.status.success(), "{} {:?}: {}", program, args, stderr(&out));
        out
    }

    /// Invoked like the plugin does: from `/`, with the directory as argument.
    fn ws(&self, args: &[&str], dir: &Path) -> Output {
        self.cmd("sh", Path::new("/"))
            .args(["-c", WS_SH, "ws.sh"])
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
    }

    fn info(&self, cwd: &Path) -> Vec<String> {
        let out = self.ws(&["info"], cwd);
        assert!(out.status.success(), "info: {}", stderr(&out));
        let line = String::from_utf8(out.stdout).unwrap();
        line.trim_end().split('\t').map(String::from).collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn s(p: &Path) -> String {
    p.to_str().unwrap().to_string()
}

#[test]
fn jj_workspace_add_info_remove_keeps_changes() {
    let sb = Sandbox::new("jj");
    let main = sb.dir.join("proj");
    sb.run("jj", &["git", "init", "proj"], &sb.dir);
    fs::write(main.join("f"), "a").unwrap();
    sb.run("jj", &["commit", "-m", "init"], &main);

    assert_eq!(sb.info(&main), ["jj", &s(&main), &s(&main), "default"]);

    let dir = sb.dir.join("proj.ws").join("feat");
    let out = sb.ws(&["add", "jj", &s(&dir), "feat"], &main);
    assert!(out.status.success(), "add: {}", stderr(&out));

    // probed from the workspace and from a subdirectory of it
    fs::create_dir(dir.join("sub")).unwrap();
    for cwd in [dir.clone(), dir.join("sub")] {
        assert_eq!(sb.info(&cwd), ["jj", &s(&main), &s(&dir), "feat"]);
    }

    fs::write(dir.join("wip"), "unsaved work").unwrap();
    let out = sb.ws(&["remove", "jj", &s(&dir), "feat"], &main);
    assert!(out.status.success(), "remove: {}", stderr(&out));

    assert!(!dir.exists());
    let list = sb.run("jj", &["workspace", "list", "-T", "name ++ \"\\n\""], &main);
    assert_eq!(String::from_utf8_lossy(&list.stdout).trim(), "default");
    // the edit made in the removed workspace (never snapshotted by a jj
    // command there) still exists as a change
    let log = sb.run("jj", &["log", "--no-graph", "-r", "files(\"wip\")", "-T", "change_id"], &main);
    assert!(!log.stdout.is_empty(), "edit made in the removed workspace was lost");
}

#[test]
fn jj_main_checkout_found_when_no_workspace_is_named_default() {
    // Same failure as a main workspace without a recorded path (repos from
    // older jj): `jj workspace root --name default` errors.
    let sb = Sandbox::new("jj-renamed");
    let main = sb.dir.join("proj");
    sb.run("jj", &["git", "init", "proj"], &sb.dir);
    sb.run("jj", &["workspace", "rename", "trunk"], &main);

    let dir = sb.dir.join("proj.ws").join("feat");
    let out = sb.ws(&["add", "jj", &s(&dir), "feat"], &main);
    assert!(out.status.success(), "add: {}", stderr(&out));

    assert_eq!(sb.info(&main), ["jj", &s(&main), &s(&main), "trunk"]);
    assert_eq!(sb.info(&dir), ["jj", &s(&main), &s(&dir), "feat"]);
}

#[test]
fn jj_remove_bookmarks_the_work_and_reopening_continues_it() {
    let sb = Sandbox::new("jj-bookmark");
    let main = sb.dir.join("proj");
    sb.run("jj", &["git", "init", "proj"], &sb.dir);
    fs::write(main.join("f"), "a").unwrap();
    sb.run("jj", &["commit", "-m", "init"], &main);
    let ws = sb.dir.join("proj.ws");
    let text = |out: Output| String::from_utf8_lossy(&out.stdout).trim().to_string();

    // committed work plus an unsnapshotted edit on top
    let dir = ws.join("feat");
    assert!(sb.ws(&["add", "jj", &s(&dir), "feat"], &main).status.success());
    fs::write(dir.join("one"), "1").unwrap();
    sb.run("jj", &["commit", "-m", "one"], &dir);
    fs::write(dir.join("two"), "2").unwrap();
    let out = sb.ws(&["remove", "jj", &s(&dir), "feat"], &main);
    assert!(out.status.success(), "remove: {}", stderr(&out));
    let files = sb.run("jj", &["file", "list", "-r", "feat"], &main);
    assert_eq!(text(files), "f\none\ntwo", "bookmark feat must hold all of the work");

    // reopening by name continues on top of the bookmark
    assert!(sb.ws(&["add", "jj", &s(&dir), "feat"], &main).status.success());
    let parent = sb.run("jj", &["log", "--no-graph", "-r", "@-", "-T", "bookmarks"], &dir);
    assert_eq!(text(parent), "feat");
    assert!(dir.join("two").exists());

    // a workspace without work of its own gets no bookmark
    let idle = ws.join("idle");
    assert!(sb.ws(&["add", "jj", &s(&idle), "idle"], &main).status.success());
    assert!(sb.ws(&["remove", "jj", &s(&idle), "idle"], &main).status.success());
    let list = sb.run("jj", &["bookmark", "list", "-T", "name ++ \"\\n\""], &main);
    assert_eq!(text(list), "feat");

    // an unrelated bookmark with the workspace's name is not moved
    let other = ws.join("taken");
    sb.run("jj", &["bookmark", "create", "taken", "-r", "@-"], &main);
    assert!(sb.ws(&["add", "jj", &s(&other), "x"], &main).status.success());
    fs::write(other.join("w"), "w").unwrap();
    let out = sb.ws(&["remove", "jj", &s(&other), "taken"], &main);
    assert!(!out.status.success(), "moved an unrelated bookmark");
    assert!(other.exists());
}

#[test]
fn list_shows_other_workspaces_on_disk() {
    let sb = Sandbox::new("list");
    let main = sb.dir.join("proj");
    sb.run("jj", &["git", "init", "proj"], &sb.dir);
    let gmain = sb.dir.join("gproj");
    sb.run("git", &["init", "-q", "gproj"], &sb.dir);
    sb.run("git", &["commit", "-q", "--allow-empty", "-m", "init"], &gmain);

    for (vcs, m) in [("jj", &main), ("git", &gmain)] {
        let a = m.with_extension("ws").join("a");
        let b = m.with_extension("ws").join("b");
        assert!(sb.ws(&["add", vcs, &s(&a), "a"], m).status.success());
        assert!(sb.ws(&["add", vcs, &s(&b), "b"], m).status.success());
        let out = sb.ws(&["list"], &b);
        assert!(out.status.success(), "{} list: {}", vcs, stderr(&out));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("a\t{}\nb\t{}\n", s(&a), s(&b)),
            "{}: main checkout excluded, from any workspace",
            vcs
        );
    }
}

#[test]
fn add_reopens_an_existing_workspace_and_refuses_foreign_directories() {
    let sb = Sandbox::new("reopen");
    let jj_main = sb.dir.join("proj");
    sb.run("jj", &["git", "init", "proj"], &sb.dir);
    let git_main = sb.dir.join("gproj");
    sb.run("git", &["init", "-q", "gproj"], &sb.dir);
    sb.run("git", &["commit", "-q", "--allow-empty", "-m", "init"], &git_main);

    for (vcs, main) in [("jj", &jj_main), ("git", &git_main)] {
        let ws = main.with_extension("ws");
        let dir = ws.join("feat");
        let first = sb.ws(&["add", vcs, &s(&dir), "feat"], main);
        assert!(first.status.success(), "{} add: {}", vcs, stderr(&first));
        fs::write(dir.join("kept"), "x").unwrap();

        let again = sb.ws(&["add", vcs, &s(&dir), "feat"], main);
        assert!(again.status.success(), "{} reopen: {}", vcs, stderr(&again));
        assert!(dir.join("kept").exists(), "{}: reopen touched the workspace", vcs);

        // an unrelated directory at the target path is not adopted
        let foreign = ws.join("foreign");
        fs::create_dir_all(&foreign).unwrap();
        let out = sb.ws(&["add", vcs, &s(&foreign), "foreign"], main);
        assert!(!out.status.success(), "{}: adopted a foreign directory", vcs);
    }

    let list = sb.run("jj", &["workspace", "list", "-T", "name ++ \"\\n\""], &jj_main);
    assert_eq!(String::from_utf8_lossy(&list.stdout).lines().count(), 2);
    let wt = sb.run("git", &["worktree", "list"], &git_main);
    assert_eq!(String::from_utf8_lossy(&wt.stdout).lines().count(), 2);
}

#[test]
fn git_worktree_add_info_remove() {
    let sb = Sandbox::new("git");
    let main = sb.dir.join("proj");
    sb.run("git", &["init", "-q", "proj"], &sb.dir);
    sb.run("git", &["commit", "-q", "--allow-empty", "-m", "init"], &main);

    assert_eq!(sb.info(&main), ["git", &s(&main), &s(&main), "main"]);

    // branch-style name: new branch keeps the slash, directory does not
    let dir = sb.dir.join("proj.ws").join("user-feat");
    let out = sb.ws(&["add", "git", &s(&dir), "user/feat"], &main);
    assert!(out.status.success(), "add: {}", stderr(&out));
    assert_eq!(sb.info(&dir), ["git", &s(&main), &s(&dir), "user-feat"]);
    let branch = sb.run("git", &["branch", "--show-current"], &dir);
    assert_eq!(String::from_utf8_lossy(&branch.stdout).trim(), "user/feat");

    // dirty worktrees are refused
    fs::write(dir.join("untracked"), "x").unwrap();
    let out = sb.ws(&["remove", "git", &s(&dir), "user-feat"], &main);
    assert!(!out.status.success());
    assert!(dir.exists());

    fs::remove_file(dir.join("untracked")).unwrap();
    let out = sb.ws(&["remove", "git", &s(&dir), "user-feat"], &main);
    assert!(out.status.success(), "remove: {}", stderr(&out));
    assert!(!dir.exists());

    // an existing branch is checked out instead of created
    let again = sb.dir.join("proj.ws").join("again");
    let out = sb.ws(&["add", "git", &s(&again), "user/feat"], &main);
    assert!(out.status.success(), "re-add: {}", stderr(&out));
}

#[test]
fn info_outside_a_repository_exits_3() {
    let sb = Sandbox::new("none");
    let out = sb.ws(&["info"], &sb.dir);
    assert_eq!(out.status.code(), Some(3));
    assert!(out.stdout.is_empty());
}
