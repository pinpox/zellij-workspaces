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
