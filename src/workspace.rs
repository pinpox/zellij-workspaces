//! jj workspace / git worktree support: naming tabs after the workspace of
//! their shell, and the create/remove dialogs. VCS work runs on the host
//! through the embedded `ws.sh` (plugins are sandboxed from the repository).

use std::collections::BTreeMap;

pub const WS_SH: &str = include_str!("ws.sh");

/// `ws.sh info` exits with this when the directory is not in a repository.
pub const NOT_A_REPO: i32 = 3;

/// One line of `ws.sh info` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WsInfo {
    /// "jj" or "git"
    pub vcs: String,
    /// root of the default workspace / main worktree
    pub main: String,
    /// root of the workspace containing the probed directory
    pub root: String,
    /// workspace name (jj) or worktree directory name / "main" (git)
    pub name: String,
}

impl WsInfo {
    pub fn project(&self) -> &str {
        basename(&self.main)
    }

    pub fn is_main(&self) -> bool {
        self.root == self.main
    }

    /// Tab name grouping this workspace under its project: `project<sep>name`.
    pub fn tab_name(&self, separator: char) -> String {
        format!("{}{}{}", self.project(), separator, self.name)
    }
}

pub fn parse_info(stdout: &str) -> Option<WsInfo> {
    let line = stdout.lines().next()?;
    let mut fields = line.split('\t');
    let info = WsInfo {
        vcs: fields.next()?.to_string(),
        main: fields.next()?.to_string(),
        root: fields.next()?.to_string(),
        name: fields.next()?.to_string(),
    };
    let valid = matches!(info.vcs.as_str(), "jj" | "git")
        && fields.next().is_none()
        && !info.main.is_empty()
        && !info.root.is_empty()
        && !info.name.is_empty();
    valid.then_some(info)
}

pub fn basename(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

/// Directory name for a requested workspace name; `/` (branch-style names)
/// becomes `-`. None for names that would be empty or hidden.
pub fn slug(name: &str) -> Option<String> {
    let s = name.trim().replace('/', "-");
    (!s.is_empty() && !s.starts_with('.')).then_some(s)
}

/// Where a new workspace goes: next to the main checkout, in `<project>.ws/`,
/// so it never shows up inside the main checkout's tree.
pub fn workspace_dir(main: &str, slug: &str) -> String {
    let main = main.trim_end_matches('/');
    let (parent, project) = main.rsplit_once('/').unwrap_or(("", main));
    format!("{}/{}.ws/{}", parent, project, slug)
}

/// Keybindings that launch this plugin's create/remove dialogs as floating
/// panes. `LaunchPlugin` fills the dialog's cwd from the focused pane, which
/// is how the dialog knows which repository it is about.
pub fn keybind_kdl(plugin_url: &str, new_key: &str, close_key: &str) -> Option<String> {
    let url = kdl_escape(plugin_url);
    let binds: Vec<String> = [(new_key, "new"), (close_key, "close")]
        .iter()
        .filter(|(key, _)| !key.trim().is_empty())
        .map(|(key, mode)| {
            format!(
                "        bind \"{}\" {{ LaunchPlugin \"{}\" {{ floating true; mode \"{}\"; }}; }}\n",
                kdl_escape(key),
                url,
                mode
            )
        })
        .collect();
    if binds.is_empty() {
        return None;
    }
    Some(format!(
        "keybinds {{\n    shared_except \"locked\" {{\n{}    }}\n}}\n",
        binds.concat()
    ))
}

fn kdl_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Run `ws.sh <dir> <args>` on the host; the result arrives as a
/// `RunCommandResult` event carrying `context`. The command itself starts in
/// `/` because zellij rewrites some cwd prefixes (e.g. `/tmp`) to plugin
/// sandbox paths; `ws.sh` changes into `dir` itself.
#[cfg(not(test))]
pub fn run_ws(args: &[&str], dir: &str, context: BTreeMap<String, String>) {
    let mut cmd = vec!["sh", "-c", WS_SH, "ws.sh", dir];
    cmd.extend_from_slice(args);
    zellij_tile::prelude::run_command_with_env_variables_and_cwd(
        &cmd,
        BTreeMap::new(),
        std::path::PathBuf::from("/"),
        context,
    );
}

#[cfg(test)]
pub fn run_ws(_args: &[&str], _dir: &str, _context: BTreeMap<String, String>) {}

/// The one-shot floating dialog's state.
#[derive(Debug, PartialEq, Default)]
pub enum Dialog {
    /// waiting for permissions / the `ws.sh info` probe
    #[default]
    Probing,
    /// create: typing the new workspace name
    Prompt { info: WsInfo, input: String },
    /// remove: waiting for y/N
    Confirm { info: WsInfo },
    /// the add/remove command is running
    Running(String),
    /// something failed or was refused; any key closes
    Error(String),
}

#[cfg(test)]
#[path = "ws_sh_tests.rs"]
mod ws_sh_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn info(main: &str, root: &str, name: &str) -> WsInfo {
        WsInfo { vcs: "jj".into(), main: main.into(), root: root.into(), name: name.into() }
    }

    #[test]
    fn parse_info_accepts_one_well_formed_line() {
        assert_eq!(
            parse_info("jj\t/c/proj\t/c/proj.ws/feat\tfeat\n"),
            Some(info("/c/proj", "/c/proj.ws/feat", "feat"))
        );
        // paths with spaces survive; only tabs separate fields
        assert_eq!(
            parse_info("git\t/c/my proj\t/c/my proj\tmain").map(|i| i.main),
            Some("/c/my proj".to_string())
        );
    }

    #[test]
    fn parse_info_rejects_malformed_output() {
        assert_eq!(parse_info(""), None);
        assert_eq!(parse_info("jj\t/c/proj\t/c/proj"), None); // missing name
        assert_eq!(parse_info("jj\t/c/proj\t/c/proj\tx\textra"), None);
        assert_eq!(parse_info("hg\t/c/proj\t/c/proj\tx"), None);
        assert_eq!(parse_info("jj\t\t/c/proj\tx"), None);
    }

    #[test]
    fn tab_name_groups_under_project_of_main_checkout() {
        assert_eq!(info("/c/proj", "/c/proj.ws/feat", "feat").tab_name(':'), "proj:feat");
        assert_eq!(info("/c/proj/", "/c/proj", "default").tab_name(':'), "proj:default");
    }

    #[test]
    fn is_main_only_for_the_main_checkout() {
        assert!(info("/c/proj", "/c/proj", "default").is_main());
        assert!(!info("/c/proj", "/c/proj.ws/feat", "feat").is_main());
    }

    #[test]
    fn slug_flattens_branch_names_and_rejects_empty_or_hidden() {
        assert_eq!(slug("user/feat-x"), Some("user-feat-x".to_string()));
        assert_eq!(slug("  fix  "), Some("fix".to_string()));
        assert_eq!(slug("   "), None);
        assert_eq!(slug(".hidden"), None);
    }

    #[test]
    fn workspace_dir_is_a_sibling_of_the_main_checkout() {
        assert_eq!(workspace_dir("/c/proj", "feat"), "/c/proj.ws/feat");
        assert_eq!(workspace_dir("/c/proj/", "feat"), "/c/proj.ws/feat");
        assert_eq!(workspace_dir("/proj", "feat"), "/proj.ws/feat");
    }

    #[test]
    fn keybind_kdl_binds_both_dialogs_and_skips_disabled_keys() {
        let kdl = keybind_kdl("file:/p/x.wasm", "Alt w", "Alt W").unwrap();
        assert!(kdl.contains(
            r#"bind "Alt w" { LaunchPlugin "file:/p/x.wasm" { floating true; mode "new"; }; }"#
        ));
        assert!(kdl.contains(
            r#"bind "Alt W" { LaunchPlugin "file:/p/x.wasm" { floating true; mode "close"; }; }"#
        ));
        let only_new = keybind_kdl("file:/p/x.wasm", "Alt w", "").unwrap();
        assert!(!only_new.contains("close"));
        assert_eq!(keybind_kdl("file:/p/x.wasm", "", " "), None);
    }

    #[test]
    fn keybind_kdl_output_parses_as_zellij_config() {
        let kdl = keybind_kdl(r#"file:/p/we"ird.wasm"#, "Alt w", "Alt W").unwrap();
        let config = zellij_utils::input::config::Config::from_kdl(&kdl, None);
        assert!(config.is_ok(), "{:?}\n{}", config.err(), kdl);
    }
}
