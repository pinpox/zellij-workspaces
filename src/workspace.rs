//! jj workspace / git worktree support: naming sessions after their
//! workspace, and the create/remove dialogs. VCS work runs on the host
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
    /// project name, unique among the repositories seen (`ws.sh` keeps them)
    pub project: String,
}

impl WsInfo {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn is_main(&self) -> bool {
        self.root == self.main
    }

    /// Name of this workspace's zellij session (see `session_name`).
    pub fn session_name(&self, separator: char) -> String {
        session_name(self.project(), separator, &self.name)
    }
}

/// Session name grouping workspace `name` under `project`:
/// `project<sep>name`. Session names cannot contain `/`, which jj
/// workspace names may.
pub fn session_name(project: &str, separator: char, name: &str) -> String {
    format!("{}{}{}", project, separator, name).replace('/', "-")
}

pub fn parse_info(stdout: &str) -> Option<WsInfo> {
    let line = stdout.lines().next()?;
    let mut fields = line.split('\t');
    let info = WsInfo {
        vcs: fields.next()?.to_string(),
        main: fields.next()?.to_string(),
        root: fields.next()?.to_string(),
        name: fields.next()?.to_string(),
        project: fields.next()?.to_string(),
    };
    let valid = matches!(info.vcs.as_str(), "jj" | "git")
        && fields.next().is_none()
        && !info.main.is_empty()
        && !info.root.is_empty()
        && !info.name.is_empty()
        && !info.project.is_empty();
    valid.then_some(info)
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

/// Keybindings that launch this plugin's dialogs as floating panes: `binds`
/// are (key, mode) pairs, keys left empty are skipped; `config` is passed to
/// every dialog. `LaunchPlugin` fills the dialog's cwd from the focused pane,
/// which is how the dialog knows which repository it is about.
pub fn keybind_kdl(plugin_url: &str, binds: &[(&str, &str)], config: &[(&str, &str)]) -> Option<String> {
    let url = kdl_escape(plugin_url);
    let config: String = config
        .iter()
        .map(|(k, v)| format!(" {} \"{}\";", k, kdl_escape(v)))
        .collect();
    let binds: Vec<String> = binds
        .iter()
        .filter(|(key, _)| !key.trim().is_empty())
        .map(|(key, mode)| {
            format!(
                "        bind \"{}\" {{ LaunchPlugin \"{}\" {{ floating true; mode \"{}\";{} }}; }}\n",
                kdl_escape(key),
                url,
                mode,
                config
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

/// A workspace of the repository that exists on disk (`ws.sh list`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Existing {
    pub name: String,
    pub root: String,
}

pub fn parse_list(stdout: &str) -> Vec<Existing> {
    stdout
        .lines()
        .filter_map(|line| {
            let (name, root) = line.split_once('\t')?;
            (!name.is_empty() && !root.is_empty()).then(|| Existing {
                name: name.to_string(),
                root: root.to_string(),
            })
        })
        .collect()
}

/// What Enter in the name prompt does.
#[derive(Debug, PartialEq, Eq)]
pub enum Choice {
    /// open (or switch to) an existing workspace
    Existing(Existing),
    /// create a workspace with this name
    New(String),
    Nothing,
}

/// The create dialog's name input with existing workspaces as suggestions.
/// `selected` indexes `matches()`; None means the typed name.
#[derive(Debug, PartialEq, Default)]
pub struct Prompt {
    pub input: String,
    pub existing: Vec<Existing>,
    pub selected: Option<usize>,
}

impl Prompt {
    /// Existing workspaces whose name contains the input (case-insensitive).
    pub fn matches(&self) -> Vec<&Existing> {
        let needle = self.input.trim().to_lowercase();
        self.existing
            .iter()
            .filter(|e| e.name.to_lowercase().contains(&needle))
            .collect()
    }

    /// Move through the matches; moving up from the first one returns to the
    /// typed name.
    pub fn move_selection(&mut self, delta: isize) {
        let len = self.matches().len() as isize;
        let next = self.selected.map_or(-1, |i| i as isize) + delta;
        self.selected = if len == 0 || next < 0 {
            None
        } else {
            Some(next.min(len - 1) as usize)
        };
    }

    pub fn type_char(&mut self, c: char) {
        self.input.push(c);
        self.selected = None;
    }

    pub fn backspace(&mut self) {
        self.input.pop();
        self.selected = None;
    }

    /// Complete the input to the selected (or only/first) match.
    pub fn complete(&mut self) {
        let name = {
            let matches = self.matches();
            self.selected
                .and_then(|i| matches.get(i))
                .or(matches.first())
                .map(|e| e.name.clone())
        };
        if let Some(name) = name {
            self.input = name;
            self.selected = None;
        }
    }

    pub fn choice(&self) -> Choice {
        let matches = self.matches();
        if let Some(e) = self.selected.and_then(|i| matches.get(i)) {
            return Choice::Existing((*e).clone());
        }
        let typed = self.input.trim();
        // typing an existing name exactly opens it, wherever it lives
        if let Some(e) = self.existing.iter().find(|e| e.name == typed) {
            return Choice::Existing(e.clone());
        }
        match slug(typed) {
            Some(_) => Choice::New(typed.to_string()),
            None => Choice::Nothing,
        }
    }
}

/// Something the go-to picker opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// a running session
    Session(String),
    /// a repository on disk: its path below the project root it was found
    /// in, and its absolute path
    Project { label: String, path: String },
}

impl Target {
    pub fn label(&self) -> &str {
        match self {
            Target::Session(name) => name,
            Target::Project { label, .. } => label,
        }
    }
}

/// `ws.sh projects` output: "<relative path>\t<absolute path>" per repository.
pub fn parse_projects(stdout: &str) -> Vec<Target> {
    stdout
        .lines()
        .filter_map(|line| {
            let (label, path) = line.split_once('\t')?;
            (!label.is_empty() && !path.is_empty())
                .then(|| Target::Project { label: label.to_string(), path: path.to_string() })
        })
        .collect()
}

/// Fuzzy match: the query's characters (ignoring case and whitespace) in
/// order somewhere in `hay`. Consecutive characters, characters starting a
/// word and characters in the last path component score higher, gaps lower.
/// The best of the matches starting at each occurrence of the first
/// character counts. None if the query does not match.
pub fn fuzzy_score(query: &str, hay: &str) -> Option<i64> {
    let query: Vec<char> = query
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| !c.is_whitespace())
        .collect();
    let Some(&first) = query.first() else {
        return Some(0);
    };
    let hay: Vec<char> = hay.chars().flat_map(char::to_lowercase).collect();
    let base = hay.iter().rposition(|&c| c == '/').map_or(0, |i| i + 1);
    let score_from = |start: usize| -> Option<i64> {
        let mut score = 0;
        let mut next = start;
        let mut prev: Option<usize> = None;
        for &q in &query {
            let i = (next..hay.len()).find(|&i| hay[i] == q)?;
            score += 1;
            if prev.is_some_and(|p| p + 1 == i) {
                score += 4;
            } else if prev.is_some() {
                score -= (i - next).min(6) as i64;
            }
            if i == 0 || matches!(hay[i - 1], '/' | '-' | '_' | ':' | '.' | ' ') {
                score += 3;
            }
            if i >= base {
                score += 2;
            }
            prev = Some(i);
            next = i + 1;
        }
        Some(score)
    };
    (0..hay.len())
        .filter(|&i| hay[i] == first)
        .filter_map(score_from)
        .max()
}

/// The go-to picker: running sessions and repositories on disk, filtered by
/// a fuzzy query. `selected` indexes `matches()`.
#[derive(Debug, PartialEq, Default)]
pub struct Picker {
    pub input: String,
    pub targets: Vec<Target>,
    pub selected: usize,
}

impl Picker {
    /// Targets matching the input, best first; on equal scores sessions
    /// before projects, then shorter labels. Without input, all in order.
    pub fn matches(&self) -> Vec<&Target> {
        let mut found: Vec<(i64, usize, &Target)> = self
            .targets
            .iter()
            .enumerate()
            .filter_map(|(i, t)| fuzzy_score(&self.input, t.label()).map(|s| (s, i, t)))
            .collect();
        if !self.input.trim().is_empty() {
            found.sort_by(|a, b| {
                let session = |t: &Target| !matches!(t, Target::Session(_));
                b.0.cmp(&a.0)
                    .then(session(a.2).cmp(&session(b.2)))
                    .then(a.2.label().len().cmp(&b.2.label().len()))
                    .then(a.1.cmp(&b.1))
            });
        }
        found.into_iter().map(|(_, _, t)| t).collect()
    }

    pub fn move_selection(&mut self, delta: isize) {
        let last = self.matches().len().saturating_sub(1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    pub fn type_char(&mut self, c: char) {
        self.input.push(c);
        self.selected = 0;
    }

    pub fn backspace(&mut self) {
        self.input.pop();
        self.selected = 0;
    }

    pub fn choice(&self) -> Option<Target> {
        self.matches().get(self.selected).map(|t| (*t).clone())
    }
}

/// The one-shot floating dialog's state.
#[derive(Debug, PartialEq, Default)]
pub enum Dialog {
    /// waiting for permissions / the `ws.sh info` probe
    #[default]
    Probing,
    /// create: typing a name or picking an existing workspace
    Prompt { info: WsInfo, prompt: Prompt },
    /// remove: waiting for y/N
    Confirm { info: WsInfo },
    /// go to: picking a session or a repository
    Goto(Picker),
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
        WsInfo {
            vcs: "jj".into(),
            main: main.into(),
            root: root.into(),
            name: name.into(),
            project: "proj".into(),
        }
    }

    #[test]
    fn parse_info_accepts_one_well_formed_line() {
        assert_eq!(
            parse_info("jj\t/c/proj\t/c/proj.ws/feat\tfeat\tproj\n"),
            Some(info("/c/proj", "/c/proj.ws/feat", "feat"))
        );
        // paths with spaces survive; only tabs separate fields
        assert_eq!(
            parse_info("git\t/c/my proj\t/c/my proj\tmain\tmy proj").map(|i| i.main),
            Some("/c/my proj".to_string())
        );
    }

    #[test]
    fn parse_info_rejects_malformed_output() {
        assert_eq!(parse_info(""), None);
        assert_eq!(parse_info("jj\t/c/proj\t/c/proj\tx"), None); // missing project
        assert_eq!(parse_info("jj\t/c/proj\t/c/proj\tx\tp\textra"), None);
        assert_eq!(parse_info("hg\t/c/proj\t/c/proj\tx\tp"), None);
        assert_eq!(parse_info("jj\t\t/c/proj\tx\tp"), None);
        assert_eq!(parse_info("jj\t/c/proj\t/c/proj\tx\t"), None);
    }

    #[test]
    fn session_name_groups_under_the_project() {
        assert_eq!(info("/c/proj", "/c/proj.ws/feat", "feat").session_name(':'), "proj:feat");
        assert_eq!(session_name("proj", ':', "user/fix"), "proj:user-fix");
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
    fn keybind_kdl_binds_dialogs_with_config_and_skips_disabled_keys() {
        let binds = [("Alt w", "new"), ("Alt W", "close")];
        let kdl = keybind_kdl("file:/p/x.wasm", &binds, &[("separator", "|")]).unwrap();
        assert!(kdl.contains(
            r#"bind "Alt w" { LaunchPlugin "file:/p/x.wasm" { floating true; mode "new"; separator "|"; }; }"#
        ));
        assert!(kdl.contains(
            r#"bind "Alt W" { LaunchPlugin "file:/p/x.wasm" { floating true; mode "close"; separator "|"; }; }"#
        ));
        let only_new = keybind_kdl("file:/p/x.wasm", &[("Alt w", "new"), ("", "close")], &[]).unwrap();
        assert!(!only_new.contains("close"));
        assert_eq!(keybind_kdl("file:/p/x.wasm", &[("", "new"), (" ", "close")], &[]), None);
    }

    #[test]
    fn keybind_kdl_output_parses_as_zellij_config() {
        let binds = [("Alt w", "new"), ("Alt W", "close"), ("Alt g", "goto")];
        let kdl =
            keybind_kdl(r#"file:/p/we"ird.wasm"#, &binds, &[("project_roots", r#"~/a "b""#)]).unwrap();
        let config = zellij_utils::input::config::Config::from_kdl(&kdl, None);
        assert!(config.is_ok(), "{:?}\n{}", config.err(), kdl);
    }

    fn prompt(names: &[&str]) -> Prompt {
        Prompt {
            existing: names
                .iter()
                .map(|n| Existing { name: n.to_string(), root: format!("/c/p.ws/{}", n) })
                .collect(),
            ..Default::default()
        }
    }

    fn names(p: &Prompt) -> Vec<&str> {
        p.matches().iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn parse_list_reads_name_root_lines() {
        assert_eq!(
            parse_list("feat\t/c/p.ws/feat\n\tjunk\nfix\t/c/p.ws/fix\n"),
            vec![
                Existing { name: "feat".into(), root: "/c/p.ws/feat".into() },
                Existing { name: "fix".into(), root: "/c/p.ws/fix".into() },
            ]
        );
    }

    #[test]
    fn matches_filter_by_case_insensitive_substring() {
        let mut p = prompt(&["feat-x", "Fix-y", "docs"]);
        assert_eq!(names(&p), ["feat-x", "Fix-y", "docs"]);
        p.type_char('f');
        p.type_char('I');
        assert_eq!(names(&p), ["Fix-y"]);
    }

    #[test]
    fn selection_moves_within_matches_and_back_to_typed_name() {
        let mut p = prompt(&["a", "b"]);
        p.move_selection(-1);
        assert_eq!(p.selected, None);
        p.move_selection(1);
        p.move_selection(1);
        p.move_selection(1);
        assert_eq!(p.selected, Some(1)); // clamped to the last match
        p.move_selection(-1);
        p.move_selection(-1);
        assert_eq!(p.selected, None);
        // typing drops a selection that may no longer be among the matches
        p.move_selection(1);
        p.type_char('b');
        assert_eq!(p.selected, None);
        assert_eq!(prompt(&[]).matches().len(), 0);
    }

    #[test]
    fn choice_prefers_selection_then_exact_name_then_new() {
        let mut p = prompt(&["feat-x", "fix"]);
        p.move_selection(1);
        p.move_selection(1);
        assert_eq!(p.choice(), Choice::Existing(p.existing[1].clone()));

        let mut p = prompt(&["feat-x", "fix"]);
        for c in "fix".chars() {
            p.type_char(c);
        }
        assert_eq!(p.choice(), Choice::Existing(p.existing[1].clone()));
        p.type_char('2');
        assert_eq!(p.choice(), Choice::New("fix2".into()));
        assert_eq!(prompt(&[]).choice(), Choice::Nothing);
    }

    #[test]
    fn complete_fills_in_selected_or_first_match() {
        let mut p = prompt(&["feat-x", "feat-y"]);
        p.type_char('f');
        p.complete();
        assert_eq!(p.input, "feat-x");
        let mut p = prompt(&["feat-x", "feat-y"]);
        p.move_selection(1);
        p.move_selection(1);
        p.complete();
        assert_eq!((p.input.as_str(), p.selected), ("feat-y", None));
    }

    fn project(label: &str) -> Target {
        Target::Project { label: label.to_string(), path: format!("/h/{}", label) }
    }

    #[test]
    fn fuzzy_matches_in_order_and_ranks_names_over_scattered_letters() {
        assert!(fuzzy_score("zw", "github.com/pinpox/zellij-workspaces").is_some());
        assert_eq!(fuzzy_score("wz", "zellij-workspaces"), None); // order matters
        assert_eq!(fuzzy_score("NIX OS", "pinpox/nixos"), fuzzy_score("nixos", "pinpox/nixos"));
        // the repository name beats the same letters spread over the path
        let named = fuzzy_score("nixos", "github.com/pinpox/nixos").unwrap();
        let scattered = fuzzy_score("nixos", "github.com/nix-community/os-thing").unwrap();
        assert!(named > scattered, "{} <= {}", named, scattered);
        // not fooled by an earlier partial match ("n" in "pinpox")
        assert!(fuzzy_score("nixos", "pinpox/nixos") > fuzzy_score("nixos", "pinpox/nix-os"));
    }

    #[test]
    fn picker_orders_by_score_then_sessions_first_and_clamps_selection() {
        let mut p = Picker {
            targets: vec![
                Target::Session("other:main".into()),
                Target::Session("nixos:main".into()),
                project("github.com/x/nixos-config"),
                project("github.com/pinpox/nixos"),
            ],
            ..Default::default()
        };
        assert_eq!(p.matches().len(), 4); // no input: everything, as given
        for c in "nixos".chars() {
            p.type_char(c);
        }
        let labels: Vec<&str> = p.matches().iter().map(|t| t.label()).collect();
        assert_eq!(labels[0], "nixos:main");
        assert!(!labels.contains(&"other:main"));
        p.move_selection(10);
        assert_eq!(p.choice(), Some((*p.matches().last().unwrap()).clone()));
        p.backspace();
        assert_eq!(p.selected, 0);
        p.input = "zzz".into();
        p.move_selection(1);
        assert_eq!(p.choice(), None);
    }

    #[test]
    fn parse_projects_reads_label_path_lines() {
        assert_eq!(
            parse_projects("a/b\t/h/a/b\n\tjunk\n"),
            vec![Target::Project { label: "a/b".into(), path: "/h/a/b".into() }]
        );
    }
}
