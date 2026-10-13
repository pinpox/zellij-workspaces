// In `cargo test` the plugin entry points are gated out (see below), so the
// methods they call look unused — silence that only for test builds.
#![cfg_attr(test, allow(dead_code))]

mod workspace;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use workspace::{
    keybind_kdl, parse_info, parse_list, parse_projects, run_ws, session_name, slug,
    workspace_dir, Choice, Dialog, Picker, Prompt, Target, WsInfo, NOT_A_REPO,
};
use zellij_tile::prelude::*;

/// Blank lines above the tree / spaces to the left of each row (breathing room).
const TOP_PAD: usize = 1;
const LEFT_PAD: usize = 1;

/// Attention is encoded as a suffix on the tab NAME — global state that every
/// sidebar instance reads identically (no per-instance divergence). We add it on
/// the attention pipe and strip it when the tab is focused; both are `rename_tab`
/// calls that mutate the shared tab name.
const MARK_WAITING: &str = " ⏳";
const MARK_COMPLETED: &str = " ✅";
/// Ongoing-work state (Claude is running): unlike the attention marks above it
/// is NOT cleared on focus — it ends when a waiting/completed/clear pipe arrives.
const MARK_WORKING: &str = " ⚙";

/// Default sidebar animation frames for `MARK_WORKING` (the tab name carries
/// only the static marker; the spinner lives purely in the render). Dense
/// braille (7 of 8 dots lit) fills the cell evenly, unlike the sparse ⠋⠙⠹ set
/// which sits visibly high-and-left next to ◆/✓. Override with the `spinner`
/// config key (each char = one frame; width-1 glyphs only).
const SPINNER_DEFAULT: &str = "⣾⣽⣻⢿⡿⣟⣯⣷";
const SPINNER_INTERVAL: f64 = 0.15;
/// How often agent terminal titles are read (see `poll_agents`).
const POLL_INTERVAL: f64 = 1.0;

/// Each char of the `spinner` config value is one animation frame.
fn parse_spinner(config: Option<&String>) -> Vec<String> {
    let frames: Vec<String> = config
        .map(String::as_str)
        .unwrap_or(SPINNER_DEFAULT)
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(String::from)
        .collect();
    if frames.is_empty() {
        SPINNER_DEFAULT.chars().map(String::from).collect()
    } else {
        frames
    }
}

/// Group order + collapse state, shared across all sidebar instances of all
/// sessions and across restarts. `/cache` is mounted per plugin *location*
/// (host side: `~/.cache/zellij/<location>/plugin_cache`), so every instance
/// in every session sees the same file; each re-reads it on `SessionUpdate`.
/// One file for all sessions, since every sidebar lists every session.
const STATE_FILE: &str = "/cache/sessions";
/// Delay between switching the client away from a session and killing it:
/// killing it first would end the client with it.
const LEAVE_DELAY: f64 = 0.5;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Attention {
    Waiting,
    Working,
    Completed,
}

fn marker_of(att: Attention) -> &'static str {
    match att {
        Attention::Waiting => MARK_WAITING,
        Attention::Working => MARK_WORKING,
        Attention::Completed => MARK_COMPLETED,
    }
}

/// Zellij's generated session names are `adjective-noun`, e.g.
/// "gregarious-cactus". Such a session gets named after its workspace.
fn is_generated_session_name(name: &str) -> bool {
    name.split_once('-').is_some_and(|(a, b)| {
        [a, b].iter().all(|w| !w.is_empty() && w.bytes().all(|c| c.is_ascii_lowercase()))
    })
}

#[derive(Debug, PartialEq, Eq)]
enum EmptyTab {
    Close,
    /// the session's last tab: leave the session (and end it)
    Leave,
}

/// What to do about the sidebar's own tab, given whether it ever had other
/// panes, how many it has now, and how many tabs the session has.
fn empty_tab_action(had_content: bool, others: usize, tabs: usize) -> Option<EmptyTab> {
    if !had_content || others > 0 {
        None
    } else if tabs <= 1 {
        Some(EmptyTab::Leave)
    } else {
        Some(EmptyTab::Close)
    }
}

/// Rollup priority for collapsed group headers: waiting > working > completed.
fn merge(acc: Option<Attention>, x: Attention) -> Attention {
    match (acc, x) {
        (Some(Attention::Waiting), _) | (_, Attention::Waiting) => Attention::Waiting,
        (Some(Attention::Working), _) | (_, Attention::Working) => Attention::Working,
        _ => Attention::Completed,
    }
}

/// A marker change for the tab containing a pane: the `zellij pipe` signals,
/// also derived from agent terminal titles.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Signal {
    Waiting,
    Working,
    Completed,
    ClearWorking,
}

fn parse_signal(s: &str) -> Option<Signal> {
    match s {
        "waiting" => Some(Signal::Waiting),
        "working" => Some(Signal::Working),
        "completed" => Some(Signal::Completed),
        "clear-working" => Some(Signal::ClearWorking),
        _ => None,
    }
}

/// Agent run state read from a pane's terminal title. Ordered by priority
/// for a tab with several agents.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum AgentTitle {
    Idle,
    Working,
    Attention,
}

/// omp's terminal title with `tui.titleState` (on by default):
/// `π <state>[ <session title>]`, where the state is `!` (needs input), `>`
/// (idle) or a spinner frame (`:` on terminals with static titles) while it
/// works. Without the state the title is `π` or `π: <session title>`.
fn parse_agent_title(title: &str) -> Option<AgentTitle> {
    let mut rest = title.strip_prefix("π ")?.chars();
    let state = rest.next()?;
    if !matches!(rest.next(), None | Some(' ')) {
        return None;
    }
    Some(match state {
        '!' => AgentTitle::Attention,
        '>' => AgentTitle::Idle,
        _ => AgentTitle::Working,
    })
}

/// The marker change for a tab whose agent state went from `prev` to `now`
/// (None: no agent title in the tab). Finishing work marks the tab done;
/// an agent that is idle from the start, or stops after asking, does not.
fn agent_signal(prev: Option<AgentTitle>, now: Option<AgentTitle>) -> Option<Signal> {
    if prev == now {
        return None;
    }
    match now {
        Some(AgentTitle::Attention) => Some(Signal::Waiting),
        Some(AgentTitle::Working) => Some(Signal::Working),
        Some(AgentTitle::Idle) if prev == Some(AgentTitle::Working) => Some(Signal::Completed),
        Some(AgentTitle::Idle) | None => prev.map(|_| Signal::ClearWorking),
    }
}

/// Persisted sidebar state: group display order, collapsed groups, and — only
/// for groups the user explicitly reordered sessions in — per-group session
/// label order. Groups absent from `session_order` list sessions by name.
#[derive(Default, PartialEq, Debug)]
struct Persisted {
    order: Vec<String>,
    collapsed: BTreeSet<String>,
    session_order: BTreeMap<String, Vec<String>>,
}

/// One line per entry: `order <group>` / `collapsed <group>` /
/// `sessionorder <group>\t<label>` (labels of one group in order, one per line).
/// Names may contain anything but a newline (and, for groups, a tab).
/// Unknown lines are ignored.
fn parse_state(s: &str) -> Persisted {
    let mut p = Persisted::default();
    for line in s.lines() {
        if let Some(g) = line.strip_prefix("order ") {
            p.order.push(g.to_string());
        } else if let Some(g) = line.strip_prefix("collapsed ") {
            p.collapsed.insert(g.to_string());
        } else if let Some(rest) = line.strip_prefix("sessionorder ") {
            if let Some((g, label)) = rest.split_once('\t') {
                p.session_order
                    .entry(g.to_string())
                    .or_default()
                    .push(label.to_string());
            }
        }
    }
    p
}

fn serialize_state(p: &Persisted) -> String {
    let mut out = String::new();
    for g in &p.order {
        out.push_str("order ");
        out.push_str(g);
        out.push('\n');
    }
    for g in &p.collapsed {
        out.push_str("collapsed ");
        out.push_str(g);
        out.push('\n');
    }
    for (g, labels) in &p.session_order {
        for label in labels {
            out.push_str("sessionorder ");
            out.push_str(g);
            out.push('\t');
            out.push_str(label);
            out.push('\n');
        }
    }
    out
}

/// Stable-sort items into saved label order; labels not in `saved` keep their
/// native relative order after the saved ones (and duplicates stay stable).
fn sort_by_saved(items: &mut [Item], saved: &[String]) {
    items.sort_by_key(|it| {
        saved
            .iter()
            .position(|l| l == &it.label)
            .unwrap_or(usize::MAX)
    });
}

/// Saved order first (dropping groups that no longer exist), then any new
/// groups in first-appearance order.
fn merge_order(saved: &[String], appearance: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = saved
        .iter()
        .filter(|g| appearance.contains(g))
        .cloned()
        .collect();
    for g in appearance {
        if !out.contains(&g) {
            out.push(g);
        }
    }
    out
}

/// Rename the first occurrence of `old` in a saved label order.
fn rename_label(order: &mut [String], old: &str, new: &str) {
    if let Some(l) = order.iter_mut().find(|l| l.as_str() == old) {
        *l = new.to_string();
    }
}

/// Swap `name` with its neighbor `delta` steps away; false if absent or at the edge.
fn move_in(order: &mut [String], name: &str, delta: isize) -> bool {
    let Some(i) = order.iter().position(|g| g == name) else {
        return false;
    };
    let j = i as isize + delta;
    if j < 0 || j as usize >= order.len() {
        return false;
    }
    order.swap(i, j as usize);
    true
}

/// Split the attention marker off a tab name: "work:api ⏳" -> (Waiting, "work:api").
fn parse_attention(name: &str) -> (Option<Attention>, &str) {
    if let Some(base) = name.strip_suffix(MARK_WAITING) {
        (Some(Attention::Waiting), base)
    } else if let Some(base) = name.strip_suffix(MARK_COMPLETED) {
        (Some(Attention::Completed), base)
    } else if let Some(base) = name.strip_suffix(MARK_WORKING) {
        (Some(Attention::Working), base)
    } else {
        (None, name)
    }
}

#[derive(Default)]
struct State {
    /// tabs of the own session
    tabs: Vec<TabInfo>,
    /// terminal pane id -> tab position (rebuilt on each PaneUpdate)
    pane_tab: BTreeMap<u32, usize>,
    /// live sessions, the own one included, as of the last SessionUpdate
    sessions: Vec<SessionInfo>,
    /// a SessionUpdate has arrived
    sessions_known: bool,
    /// former names of the own session that SessionUpdate may still list as
    /// another session (zellij keeps the entry after a rename until the
    /// session list changes)
    renamed_from: BTreeSet<String>,
    /// the own session has a client attached, so its active tab is seen
    attached: bool,
    collapsed: BTreeSet<String>,
    /// saved group display order; groups not listed here follow in first-appearance order
    group_order: Vec<String>,
    /// per-group saved session label order; groups not present list sessions by name
    session_order: BTreeMap<String, Vec<String>>,
    /// name of the own session (from ModeUpdate)
    session: Option<String>,
    selected: usize,
    /// row of the own session at the last update; the selection follows it
    current_row: Option<usize>,
    separator: char,
    waiting_icon: String,
    completed_icon: String,
    /// what this instance is: the per-tab sidebar or a one-shot dialog
    mode: Mode,
    plugin_id: u32,
    /// position of the tab this instance's own pane lives in, and its URL
    /// (both from PaneUpdate)
    own_tab: Option<usize>,
    own_url: Option<String>,
    /// tiled terminal panes of the own tab, in manifest order
    own_terminal_panes: Vec<u32>,
    /// panes in the own tab besides this sidebar, as of the last PaneUpdate
    own_tab_others: usize,
    /// the own tab has had other panes (a fresh tab may report the sidebar
    /// before its terminal exists)
    own_tab_had_content: bool,
    /// close/leave already requested for the own tab
    closing: bool,
    /// session to kill once the client has switched away from it, and
    /// whether the timer that kills it is the one pending
    leaving: Option<String>,
    leave_armed: bool,
    granted: bool,
    keys_bound: bool,
    new_key: String,
    close_key: String,
    goto_key: String,
    /// where the go-to picker looks for repositories (`:`-separated), and
    /// how many directories deep
    project_roots: String,
    project_depth: String,
    /// the own session's name was checked for renaming after its workspace
    autonamed: bool,
    dialog: Dialog,
    /// active inline rename edit: (target, input buffer)
    renaming: Option<(RenameTarget, String)>,
    /// animation frames (from the `spinner` config key), frame counter, and
    /// whether a Timer event is already scheduled
    spinner: Vec<String>,
    spin: usize,
    timer_running: bool,
    /// interval of the scheduled timer, and time since titles were last read
    timer_interval: f64,
    since_poll: f64,
    /// agent title state of each terminal pane showing one, as of the last
    /// title read
    agents: BTreeMap<u32, AgentTitle>,
}

enum RenameTarget {
    Group(String),
    /// session name; edits its label (group prefix is kept)
    Session(String),
}

// The plugin entry points call zellij-tile's wasm host imports, which don't exist
// on the host target — gate them out of `cargo test` so the pure logic can be tested.
#[cfg(not(test))]
register_plugin!(State);

/// A session within a group, while building the tree.
struct Item {
    session: String,
    label: String,
    current: bool,
    attention: Option<Attention>,
}

enum Row {
    Group { name: String, collapsed: bool, count: usize, attention: Option<Attention> },
    Session { session: String, label: String, current: bool, attention: Option<Attention> },
}

/// The most urgent marker on any of the tabs.
fn rollup(tabs: &[TabInfo]) -> Option<Attention> {
    tabs.iter().fold(None, |acc, t| match parse_attention(&t.name).0 {
        Some(x) => Some(merge(acc, x)),
        None => acc,
    })
}

impl State {
    fn group_of<'a>(&self, name: &'a str) -> (String, &'a str) {
        match name.find(self.separator) {
            Some(i) => (
                name[..i].to_string(),
                name[i + self.separator.len_utf8()..].trim_start(),
            ),
            None => ("General".to_string(), name),
        }
    }

    /// Every live session with the rollup of its tabs' markers, by name. The
    /// own session's tabs come from TabUpdate: SessionUpdate lags up to a
    /// second behind, and may still carry the own session's old name after
    /// a rename.
    fn session_summaries(&self) -> Vec<(String, Option<Attention>)> {
        let mut out: Vec<(String, Option<Attention>)> = self
            .other_sessions()
            .map(|s| (s.name.clone(), rollup(&s.tabs)))
            .collect();
        if let Some(own) = &self.session {
            out.push((own.clone(), rollup(&self.tabs)));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Live sessions other than the own one.
    fn other_sessions(&self) -> impl Iterator<Item = &SessionInfo> {
        self.sessions.iter().filter(|s| {
            !s.is_current_session
                && Some(&s.name) != self.session.as_ref()
                && !self.renamed_from.contains(&s.name)
        })
    }

    fn is_live(&self, name: &str) -> bool {
        self.session.as_deref() == Some(name) || self.other_sessions().any(|s| s.name == name)
    }

    /// Take the own session's name from ModeUpdate; true if it changed.
    fn set_session_name(&mut self, name: Option<String>) -> bool {
        if name == self.session {
            return false;
        }
        if let Some(old) = std::mem::replace(&mut self.session, name) {
            self.renamed_from.insert(old);
        }
        true
    }

    /// Groups in first-appearance (session name) order.
    fn appearance_groups(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (name, _) in self.session_summaries() {
            let (g, _) = self.group_of(&name);
            if !out.contains(&g) {
                out.push(g);
            }
        }
        out
    }

    fn display_group_order(&self) -> Vec<String> {
        merge_order(&self.group_order, self.appearance_groups())
    }

    fn load_state(&mut self) {
        if let Ok(s) = std::fs::read_to_string(STATE_FILE) {
            let p = parse_state(&s);
            self.group_order = p.order;
            self.collapsed = p.collapsed;
            self.session_order = p.session_order;
        }
    }

    fn save_state(&self) {
        let p = Persisted {
            order: self.display_group_order(),
            collapsed: self.collapsed.clone(),
            session_order: self.session_order.clone(),
        };
        let _ = std::fs::write(STATE_FILE, serialize_state(&p));
    }

    /// The persisted fields as they are in memory, for change detection.
    fn persisted(&self) -> Persisted {
        Persisted {
            order: self.group_order.clone(),
            collapsed: self.collapsed.clone(),
            session_order: self.session_order.clone(),
        }
    }

    /// Groups in display order, each with its sessions in display order.
    fn grouped_items(&self) -> Vec<(String, Vec<Item>)> {
        let mut groups: BTreeMap<String, Vec<Item>> = BTreeMap::new();
        for (name, attention) in self.session_summaries() {
            let (g, label) = self.group_of(&name);
            let item = Item {
                label: label.to_string(),
                current: self.session.as_ref() == Some(&name),
                attention,
                session: name.clone(),
            };
            groups.entry(g).or_default().push(item);
        }
        self.display_group_order()
            .iter()
            .filter_map(|g| {
                let mut items = groups.remove(g)?;
                if let Some(saved) = self.session_order.get(g) {
                    sort_by_saved(&mut items, saved);
                }
                Some((g.clone(), items))
            })
            .collect()
    }

    fn build_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (g, items) in self.grouped_items() {
            let collapsed = self.collapsed.contains(&g);
            let group_att = items.iter().fold(None, |acc, it| match it.attention {
                Some(x) => Some(merge(acc, x)),
                None => acc,
            });
            rows.push(Row::Group {
                name: g,
                collapsed,
                count: items.len(),
                attention: group_att,
            });
            if !collapsed {
                for it in items {
                    rows.push(Row::Session {
                        session: it.session,
                        label: it.label,
                        current: it.current,
                        attention: it.attention,
                    });
                }
            }
        }
        rows
    }

    /// Keep the selection on the own session's row when that row moves, and
    /// within the rows otherwise.
    fn sync_selection(&mut self) {
        let rows = self.build_rows();
        let current = rows
            .iter()
            .position(|r| matches!(r, Row::Session { current: true, .. }));
        if let Some(i) = current.filter(|_| current != self.current_row) {
            self.selected = i;
        }
        self.current_row = current;
        if !rows.is_empty() && self.selected >= rows.len() {
            self.selected = rows.len() - 1;
        }
    }

    fn icon(&self, att: Option<Attention>) -> String {
        match att {
            Some(Attention::Waiting) => format!("\u{1b}[31m{}\u{1b}[39m ", self.waiting_icon),
            Some(Attention::Working) => {
                let frame = self
                    .spinner
                    .get(self.spin % self.spinner.len().max(1))
                    .map(String::as_str)
                    .unwrap_or("⣾");
                format!("\u{1b}[33m{}\u{1b}[39m ", frame)
            }
            Some(Attention::Completed) => format!("\u{1b}[36m{}\u{1b}[39m ", self.completed_icon),
            None => String::new(),
        }
    }

    fn any_working(&self) -> bool {
        self.session_summaries()
            .iter()
            .any(|(_, att)| *att == Some(Attention::Working))
    }

    /// Keep one timer running once permitted: at the spinner's rate while a
    /// session is working, else at the title poll's.
    fn ensure_timer(&mut self) {
        if self.timer_running || !self.granted || self.leaving.is_some() {
            return;
        }
        self.timer_interval = if self.any_working() { SPINNER_INTERVAL } else { POLL_INTERVAL };
        set_timeout(self.timer_interval);
        self.timer_running = true;
    }

    /// Server-fresh info for the tab containing `pane_id`. Pipe-driven renames
    /// must never derive a tab's name from this instance's cached tab list: the
    /// caches of hidden instances diverge, pipes broadcast to every instance,
    /// and one stale cache writing `cached name + marker` resurrects an old tab
    /// name (user renames kept reverting). Only the pane→tab-id mapping comes
    /// from the cache (ids are stable); name and active flags are queried live,
    /// so any number of instances can handle the same pipe idempotently — which
    /// is also required, because no single instance reliably receives pipes.
    fn fresh_tab(&self, pane_id: u32) -> Option<TabInfo> {
        let tab_pos = *self.pane_tab.get(&pane_id)?;
        let tab_id = self.tabs.iter().find(|t| t.position == tab_pos)?.tab_id;
        get_tab_info(tab_id)
    }

    /// Add an attention marker to the tab containing `pane_id` (global rename).
    /// The setters return false when that tab is not known (yet).
    fn set_attention(&self, pane_id: u32, attention: Attention) -> bool {
        let Some(t) = self.fresh_tab(pane_id) else {
            return false;
        };
        let (att, base) = parse_attention(&t.name);
        // Never mark the tab you're already looking at — you don't need an
        // attention cue for it, and it keeps set/clear free of any race.
        // But a working spinner on it still has to END now. Without a client
        // on the session nobody looks at its active tab.
        if t.active && self.attached {
            if att == Some(Attention::Working) {
                rename_tab_with_id(t.tab_id as u64, base.to_string());
            }
            return true;
        }
        if att != Some(attention) {
            rename_tab_with_id(t.tab_id as u64, format!("{}{}", base, marker_of(attention)));
        }
        true
    }

    /// Mark the tab containing `pane_id` as working. Unlike attention marks
    /// this applies to the active tab too (it is not cleared on focus, so
    /// there is no set/clear race — only pipes ever end it).
    fn set_working(&self, pane_id: u32) -> bool {
        let Some(t) = self.fresh_tab(pane_id) else {
            return false;
        };
        let (att, base) = parse_attention(&t.name);
        if att != Some(Attention::Working) {
            rename_tab_with_id(t.tab_id as u64, format!("{}{}", base, MARK_WORKING));
        }
        true
    }

    /// Strip a working marker (Claude exited without a Stop) — attention marks stay.
    fn clear_working(&self, pane_id: u32) -> bool {
        let Some(t) = self.fresh_tab(pane_id) else {
            return false;
        };
        let (att, base) = parse_attention(&t.name);
        if att == Some(Attention::Working) {
            rename_tab_with_id(t.tab_id as u64, base.to_string());
        }
        true
    }

    fn apply_signal(&self, pane_id: u32, signal: Signal) -> bool {
        match signal {
            Signal::Waiting => self.set_attention(pane_id, Attention::Waiting),
            Signal::Completed => self.set_attention(pane_id, Attention::Completed),
            Signal::Working => self.set_working(pane_id),
            Signal::ClearWorking => self.clear_working(pane_id),
        }
    }

    /// Follow the agent states that terminal titles show (see
    /// `parse_agent_title`), one state per tab: its strongest agent. Every
    /// running instance handles every tab of its session, because zellij
    /// starts the sidebar of a tab opened in the background only once it is
    /// shown; the renames are idempotent, as for pipes. `titles`: (tab
    /// position, pane, title) of every terminal pane.
    fn track_agents(&mut self, titles: Vec<(usize, u32, String)>) {
        if !self.granted {
            return;
        }
        let mut by_tab: BTreeMap<usize, Vec<(u32, String)>> = BTreeMap::new();
        for (pos, pane, title) in titles {
            by_tab.entry(pos).or_default().push((pane, title));
        }
        let mut agents = BTreeMap::new();
        for panes in by_tab.values() {
            let mut prev = None;
            let mut now = None;
            let mut seen = Vec::new();
            for (pane, title) in panes {
                if let Some(&s) = self.agents.get(pane) {
                    prev = prev.max(Some((s, *pane)));
                }
                if let Some(s) = parse_agent_title(title) {
                    seen.push((*pane, s));
                    now = now.max(Some((s, *pane)));
                }
            }
            let pane = now.or(prev).map(|(_, p)| p);
            // working and waiting are kept up on every read, not just on a
            // change: a marker lost to a rename (or set before the tab list
            // arrived after a reload) comes back; the setters are no-ops
            // while it is in place
            let signal = agent_signal(prev.map(|(s, _)| s), now.map(|(s, _)| s)).or(match now {
                Some((AgentTitle::Working, _)) => Some(Signal::Working),
                Some((AgentTitle::Attention, _)) => Some(Signal::Waiting),
                _ => None,
            });
            let applied = match (signal, pane) {
                (Some(signal), Some(pane)) => self.apply_signal(pane, signal),
                _ => true,
            };
            if applied {
                agents.extend(seen);
            } else {
                // e.g. the tab list has not arrived yet after a (re)load:
                // keep the old states so the change is retried
                agents.extend(panes.iter().filter_map(|(p, _)| self.agents.get(p).map(|&s| (*p, s))));
            }
        }
        self.agents = agents;
    }

    /// Title changes don't cause a PaneUpdate, so the timer reads them.
    fn poll_agents(&mut self) {
        let titles = self
            .pane_tab
            .iter()
            .filter_map(|(&id, &pos)| get_pane_info(PaneId::Terminal(id)).map(|p| (pos, id, p.title)))
            .collect();
        self.track_agents(titles);
    }

    /// A session zellij named itself that was started inside a repository is
    /// renamed `project:workspace`, once (see `rename_after_workspace`).
    fn probe_session_name(&mut self) {
        if self.autonamed || !self.granted || !self.sessions_known {
            return;
        }
        let Some(name) = &self.session else {
            return;
        };
        if !is_generated_session_name(name) {
            self.autonamed = true;
            return;
        }
        let Some(&pane_id) = self.own_terminal_panes.first() else {
            return;
        };
        self.autonamed = true;
        if let Ok(cwd) = get_pane_cwd(PaneId::Terminal(pane_id)) {
            let context = BTreeMap::from([("op".to_string(), "name".to_string())]);
            run_ws(&["info"], &cwd.to_string_lossy(), context);
        }
    }

    /// Rename the own session after its workspace unless another session
    /// has that name already.
    fn rename_after_workspace(&self, info: &WsInfo) {
        let name = info.session_name(self.separator);
        if self.session.as_deref().is_some_and(is_generated_session_name) && !self.is_live(&name) {
            rename_session(&name);
        }
    }

    /// Register the dialogs' keybindings (in memory, not written to the
    /// user's config). Every sidebar instance binds the same keys to the same
    /// action, so repeating this per tab is harmless.
    fn bind_keys(&mut self) {
        if self.keys_bound || !self.granted {
            return;
        }
        let Some(url) = &self.own_url else {
            return;
        };
        let binds = [
            (self.new_key.as_str(), "new"),
            (self.close_key.as_str(), "close"),
            (self.goto_key.as_str(), "goto"),
        ];
        let separator = self.separator.to_string();
        let config = [
            ("separator", separator.as_str()),
            ("project_roots", self.project_roots.as_str()),
            ("project_depth", self.project_depth.as_str()),
        ];
        if let Some(kdl) = keybind_kdl(url, &binds, &config) {
            reconfigure(kdl, false);
        }
        self.keys_bound = true;
    }

    fn update_panes(&mut self, manifest: &PaneManifest) {
        self.pane_tab.clear();
        for (tab_pos, panes) in &manifest.panes {
            for p in panes {
                if !p.is_plugin {
                    self.pane_tab.insert(p.id, *tab_pos);
                } else if p.id == self.plugin_id {
                    self.own_tab = Some(*tab_pos);
                    self.own_url = p.plugin_url.clone();
                }
            }
        }
        let own_panes = self.own_tab.and_then(|pos| manifest.panes.get(&pos));
        self.own_terminal_panes = own_panes
            .map(|panes| {
                panes
                    .iter()
                    .filter(|p| !p.is_plugin && !p.is_floating)
                    .map(|p| p.id)
                    .collect()
            })
            .unwrap_or_default();
        // like zellij's own "tab is empty" rule: unselectable panes (status
        // bar, tab bar) don't keep a tab open, nor do hidden helper plugins
        // (`zellij:link` sits suppressed in the first tab) or this plugin's
        // own instances (the sidebar and an Alt w / Alt W dialog left open,
        // possibly hidden with the floating layer). Suppressed terminals do
        // count: zellij restores them when their replacement closes.
        self.own_tab_others = own_panes
            .map(|panes| {
                panes
                    .iter()
                    .filter(|p| {
                        p.is_selectable
                            && !(p.is_plugin
                                && (p.id == self.plugin_id
                                    || p.is_suppressed
                                    || p.plugin_url.is_some() && p.plugin_url == self.own_url))
                    })
                    .count()
            })
            .unwrap_or(0);
        if self.own_tab_others > 0 {
            self.own_tab_had_content = true;
        }
    }

    /// Record what a SessionUpdate says about the own session and the others.
    /// The own session's name comes from ModeUpdate only: a SessionUpdate may
    /// still carry the old one after a rename.
    fn update_sessions(&mut self, sessions: Vec<SessionInfo>) {
        if let Some(own) = sessions.iter().find(|s| s.is_current_session) {
            self.attached = own.connected_clients > 0;
            // until ModeUpdate tells; later this may be a stale name
            if self.session.is_none() {
                self.session = Some(own.name.clone());
            }
        }
        // a ghost is gone once a session list without it arrives; a session
        // of that name appearing later is a real one
        self.renamed_from.retain(|n| sessions.iter().any(|s| &s.name == n));
        self.sessions = sessions;
        self.sessions_known = true;
    }

    /// Apply a session list (SessionUpdate, or a scan of our own); true if
    /// the sidebar changed.
    fn on_sessions(&mut self, sessions: Vec<SessionInfo>) -> bool {
        let before = (self.session_summaries(), self.persisted());
        let was_attached = self.attached;
        self.update_sessions(sessions);
        // Pick up order/collapse changes written by other instances.
        self.load_state();
        // a client arrived: the tab it looks at is seen now
        if self.attached && !was_attached {
            self.clear_seen_tab();
            // the keybindings only reach clients connected at the time
            self.keys_bound = false;
            self.bind_keys();
        }
        self.sync_selection();
        self.ensure_timer();
        self.probe_session_name();
        before != (self.session_summaries(), self.persisted())
    }

    /// Zellij reads the other sessions' state only when a plugin asks for
    /// it (which also sends the result to every plugin of the session as a
    /// SessionUpdate). The sidebar on screen asks on every title poll.
    fn refresh_sessions(&mut self) -> bool {
        let on_screen = self.attached
            && self
                .own_tab
                .is_some_and(|pos| self.tabs.iter().any(|t| t.position == pos && t.active));
        if !on_screen {
            return false;
        }
        match get_session_list() {
            Ok(list) => self.on_sessions(list.live_sessions),
            Err(_) => false,
        }
    }

    /// The session to move the client to when leaving `own`: the first other
    /// one of its project in sidebar order, else the first other one at all.
    fn fallback_session(&self, own: &str) -> Option<String> {
        let (group, _) = self.group_of(own);
        let items = self.grouped_items();
        let first_other = |only: Option<&str>| {
            items
                .iter()
                .filter(|(g, _)| only.is_none_or(|o| o == g))
                .flat_map(|(_, its)| its)
                .map(|it| &it.session)
                .find(|s| s.as_str() != own)
                .cloned()
        };
        first_other(Some(&group)).or_else(|| first_other(None))
    }

    /// Switch the client to another session and end this one shortly after;
    /// without another session, start one in `fallback_cwd`, or quit.
    fn leave_session(&mut self, fallback_cwd: Option<&str>) {
        let Some(own) = self.session.clone() else {
            quit_zellij();
            return;
        };
        match (self.fallback_session(&own), fallback_cwd) {
            (Some(other), _) => switch_session(Some(&other)),
            // a new session, named by its sidebar after the workspace there
            (None, Some(cwd)) => switch_session_with_cwd(None, Some(PathBuf::from(cwd))),
            (None, None) => {
                quit_zellij();
                return;
            }
        }
        self.leaving = Some(own);
        if !self.timer_running {
            self.arm_leave();
        }
    }

    fn arm_leave(&mut self) {
        set_timeout(LEAVE_DELAY);
        self.timer_running = true;
        self.leave_armed = true;
    }

    /// A timer fired while leaving: the first one may have been set before
    /// the switch (the title poll), so the session is killed on a timer of
    /// LEAVE_DELAY set once no other one is pending. True while leaving.
    fn leave_tick(&mut self) -> bool {
        let Some(session) = self.leaving.clone() else {
            return false;
        };
        if self.leave_armed {
            kill_sessions(&[session]);
            self.leaving = None;
        } else {
            self.arm_leave();
        }
        true
    }

    /// The sidebar keeps its tab alive after the last real pane is gone, which
    /// zellij would otherwise close: close the tab, or leave and end the
    /// session when it is the last one, like zellij ends a session when its
    /// last pane exits. The workspace on disk is untouched; Alt w reopens it.
    fn close_if_empty(&mut self) {
        if self.closing {
            return;
        }
        match empty_tab_action(self.own_tab_had_content, self.own_tab_others, self.tabs.len()) {
            Some(EmptyTab::Leave) => {
                self.closing = true;
                self.leave_session(None);
            }
            Some(EmptyTab::Close) => {
                if let Some(tab_id) = self.own_tab_id() {
                    self.closing = true;
                    close_tab_with_id(tab_id as u64);
                }
            }
            None => {}
        }
    }

    fn own_tab_id(&self) -> Option<usize> {
        let pos = self.own_tab?;
        self.tabs.iter().find(|t| t.position == pos).map(|t| t.tab_id)
    }

    /// Strip a *seen* attention marker from the active tab (global rename),
    /// if a client looks at this session. A working spinner survives focus —
    /// peeking at a running task must not kill its indicator; only
    /// waiting/completed are "acknowledged by looking".
    fn clear_seen_tab(&self) {
        if !self.attached {
            return;
        }
        if let Some(t) = self.tabs.iter().find(|t| t.active) {
            let (att, base) = parse_attention(&t.name);
            if matches!(att, Some(Attention::Waiting) | Some(Attention::Completed)) {
                rename_tab_with_id(t.tab_id as u64, base.to_string());
            }
        }
    }

    fn activate(&mut self, idx: usize) -> bool {
        let rows = self.build_rows();
        match rows.get(idx) {
            Some(Row::Group { name, .. }) => {
                if !self.collapsed.remove(name) {
                    self.collapsed.insert(name.clone());
                }
                self.save_state();
                true
            }
            Some(Row::Session { session, current, .. }) => {
                if !current {
                    switch_session(Some(session));
                }
                true
            }
            None => false,
        }
    }

    /// Move the row under the selection up/down (persisted): a group header
    /// reorders the group, a session row reorders the session within its group.
    fn move_selected(&mut self, delta: isize) -> bool {
        let rows = self.build_rows();
        match rows.get(self.selected) {
            Some(Row::Group { name, .. }) => {
                let name = name.clone();
                let mut order = self.display_group_order();
                if !move_in(&mut order, &name, delta) {
                    return false;
                }
                self.group_order = order;
                self.save_state();
                // keep the selection on the header we just moved
                if let Some(idx) = self
                    .build_rows()
                    .iter()
                    .position(|r| matches!(r, Row::Group { name: n, .. } if *n == name))
                {
                    self.selected = idx;
                }
                true
            }
            Some(Row::Session { session, label, .. }) => {
                let (session, label) = (session.clone(), label.clone());
                let Some((group, items)) = self
                    .grouped_items()
                    .into_iter()
                    .find(|(_, items)| items.iter().any(|it| it.session == session))
                else {
                    return false;
                };
                let mut labels: Vec<String> = items.into_iter().map(|it| it.label).collect();
                if !move_in(&mut labels, &label, delta) {
                    return false;
                }
                let present = self.appearance_groups();
                self.session_order.retain(|g, _| present.contains(g));
                self.session_order.insert(group, labels);
                self.save_state();
                // keep the selection on the session we just moved
                if let Some(idx) = self
                    .build_rows()
                    .iter()
                    .position(|r| matches!(r, Row::Session { session: s, .. } if *s == session))
                {
                    self.selected = idx;
                }
                true
            }
            None => false,
        }
    }

    /// Move `old` group's saved order/collapse/session-order entries to `new`.
    fn migrate_group_state(&mut self, old: &str, new: &str) {
        for g in self.group_order.iter_mut() {
            if g == old {
                *g = new.to_string();
            }
        }
        // renaming into an existing group must not leave a duplicate entry
        let mut seen = BTreeSet::new();
        self.group_order.retain(|g| seen.insert(g.clone()));
        if self.collapsed.remove(old) {
            self.collapsed.insert(new.to_string());
        }
        if let Some(order) = self.session_order.remove(old) {
            self.session_order.entry(new.to_string()).or_insert(order);
        }
    }

    /// Rename a live session unless another one has the new name: the own
    /// one directly, others through the zellij CLI.
    fn rename_live_session(&self, old: &str, new: &str) {
        if old == new || self.is_live(new) {
            return;
        }
        if self.session.as_deref() == Some(old) {
            rename_session(new);
        } else {
            run_command(&["zellij", "-s", old, "action", "rename-session", new], BTreeMap::new());
        }
    }

    /// Rename group `old` to `new`: re-prefix every member session and
    /// migrate the group's saved state.
    fn commit_rename(&mut self, old: &str, new: &str) {
        let new = new.trim();
        if new.is_empty() || new == old {
            return;
        }
        for (name, _) in self.session_summaries() {
            let (g, label) = self.group_of(&name);
            if g == old {
                self.rename_live_session(&name, &format!("{}{}{}", new, self.separator, label));
            }
        }
        self.migrate_group_state(old, new);
        self.save_state();
    }

    /// Rename the label of session `session`, keeping its group prefix
    /// (ungrouped sessions stay ungrouped).
    fn commit_session_rename(&mut self, session: &str, new_label: &str) {
        let new_label = new_label.trim();
        let (g, old_label) = self.group_of(session);
        if new_label.is_empty() || new_label == old_label {
            return;
        }
        let name = if session.find(self.separator).is_none() {
            new_label.to_string()
        } else {
            format!("{}{}{}", g, self.separator, new_label)
        };
        self.rename_live_session(session, &name);
        if let Some(order) = self.session_order.get_mut(&g) {
            rename_label(order, old_label, new_label);
            self.save_state();
        }
    }

    fn handle_rename_key(&mut self, key: KeyWithModifier) -> bool {
        let Some((target, mut buf)) = self.renaming.take() else {
            return false;
        };
        let plain = key.key_modifiers.is_empty()
            || (key.key_modifiers.len() == 1 && key.key_modifiers.contains(&KeyModifier::Shift));
        match key.bare_key {
            BareKey::Enter => {
                match &target {
                    RenameTarget::Group(old) => self.commit_rename(&old.clone(), &buf),
                    RenameTarget::Session(s) => self.commit_session_rename(&s.clone(), &buf),
                }
                return true;
            }
            BareKey::Esc => return true, // buffer dropped = cancelled
            BareKey::Backspace => {
                buf.pop();
            }
            // the separator would corrupt group parsing, a tab the state file
            // format; session names cannot contain `/`
            BareKey::Char(c) if plain && !c.is_control() && c != self.separator && c != '/' => {
                buf.push(c);
            }
            _ => {}
        }
        self.renaming = Some((target, buf));
        true
    }

    fn handle_key(&mut self, key: KeyWithModifier) -> bool {
        if self.renaming.is_some() {
            return self.handle_rename_key(key);
        }
        let len = self.build_rows().len();
        if len == 0 {
            return false;
        }
        let shift = key.key_modifiers.contains(&KeyModifier::Shift);
        match key.bare_key {
            // shifted chars arrive as their uppercase form, arrows carry the modifier
            BareKey::Char('J') => self.move_selected(1),
            BareKey::Char('K') => self.move_selected(-1),
            BareKey::Down if shift => self.move_selected(1),
            BareKey::Up if shift => self.move_selected(-1),
            BareKey::Char('j') | BareKey::Down => {
                self.selected = (self.selected + 1).min(len - 1);
                true
            }
            BareKey::Char('k') | BareKey::Up => {
                self.selected = self.selected.saturating_sub(1);
                true
            }
            BareKey::Enter | BareKey::Char(' ') => {
                let sel = self.selected;
                self.activate(sel)
            }
            BareKey::Char('r') => {
                let rows = self.build_rows();
                match rows.get(self.selected) {
                    Some(Row::Group { name, .. }) => {
                        self.renaming = Some((RenameTarget::Group(name.clone()), name.clone()));
                        true
                    }
                    Some(Row::Session { session, label, .. }) => {
                        self.renaming =
                            Some((RenameTarget::Session(session.clone()), label.clone()));
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        }
    }

    fn handle_mouse(&mut self, mouse: Mouse) -> bool {
        if self.renaming.take().is_some() {
            return true; // any mouse action cancels an in-progress rename
        }
        let len = self.build_rows().len();
        if len == 0 {
            return false;
        }
        match mouse {
            Mouse::LeftClick(row, _col) => {
                let idx = row - TOP_PAD as isize;
                if idx < 0 || idx as usize >= len {
                    return false;
                }
                let idx = idx as usize;
                self.selected = idx;
                self.activate(idx);
                true
            }
            Mouse::ScrollUp(_) => {
                self.selected = self.selected.saturating_sub(1);
                true
            }
            Mouse::ScrollDown(_) => {
                self.selected = (self.selected + 1).min(len - 1);
                true
            }
            _ => false,
        }
    }
}

/// `mode` plugin config: the layout's sidebar, or a dialog launched by the
/// keybindings the sidebar registers.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Mode {
    #[default]
    Sidebar,
    New,
    Close,
    Goto,
}

fn command_error(code: Option<i32>, stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        format!("command failed (exit code {:?})", code)
    } else {
        stderr.to_string()
    }
}

impl State {
    fn update_dialog(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(status) => {
                if status == PermissionStatus::Granted {
                    let title = match self.mode {
                        Mode::Close => "remove workspace",
                        Mode::Goto => "go to",
                        _ => "new workspace",
                    };
                    rename_plugin_pane(self.plugin_id, title);
                    if let Ok(list) = get_session_list() {
                        self.update_sessions(list.live_sessions);
                    }
                    if self.mode == Mode::Goto {
                        self.start_goto();
                    } else {
                        let cwd = get_plugin_ids().initial_cwd;
                        let context = BTreeMap::from([("op".to_string(), "probe".to_string())]);
                        run_ws(&["info"], &cwd.to_string_lossy(), context);
                    }
                } else {
                    self.dialog = Dialog::Error("permissions were denied".to_string());
                }
                true
            }
            Event::TabUpdate(tabs) => {
                self.tabs = tabs;
                false
            }
            Event::ModeUpdate(mode_info) => {
                self.set_session_name(mode_info.session_name);
                false
            }
            Event::SessionUpdate(sessions, _) => {
                self.update_sessions(sessions);
                matches!(self.dialog, Dialog::Prompt { .. })
            }
            Event::Timer(_) => {
                self.timer_running = false;
                self.leave_tick();
                false
            }
            Event::RunCommandResult(code, stdout, stderr, context) => {
                self.dialog_result(code, &stdout, &stderr, &context);
                true
            }
            Event::Key(key) => self.dialog_key(key),
            _ => false,
        }
    }

    fn dialog_result(
        &mut self,
        code: Option<i32>,
        stdout: &[u8],
        stderr: &[u8],
        context: &BTreeMap<String, String>,
    ) {
        let ok = code == Some(0);
        match context.get("op").map(String::as_str) {
            Some("probe") => {
                self.dialog = match parse_info(&String::from_utf8_lossy(stdout)) {
                    Some(info) if ok => match self.mode {
                        Mode::Close if info.is_main() => Dialog::Error(format!(
                            "{} is the main checkout of {}, not removing it",
                            info.name,
                            info.project()
                        )),
                        Mode::Close => Dialog::Confirm { info },
                        _ => {
                            let context =
                                BTreeMap::from([("op".to_string(), "list".to_string())]);
                            run_ws(&["list", "all"], &info.root, context);
                            Dialog::Prompt { info, prompt: Prompt::default() }
                        }
                    },
                    _ if code == Some(NOT_A_REPO) => Dialog::Error(format!(
                        "not in a jj or git repository:\n{}",
                        get_plugin_ids().initial_cwd.display()
                    )),
                    _ => Dialog::Error(command_error(code, stderr)),
                };
            }
            Some("list") if ok => {
                if let Dialog::Prompt { prompt, .. } = &mut self.dialog {
                    prompt.existing = parse_list(&String::from_utf8_lossy(stdout));
                }
            }
            Some("add") if ok => {
                if let (Some(session), Some(dir)) = (context.get("session"), context.get("dir")) {
                    self.open_session(session, dir);
                }
                close_self();
            }
            Some("remove") if ok => {
                if let (Some(session), Some(main)) = (context.get("session"), context.get("main")) {
                    self.end_session(session, main);
                }
            }
            Some("add") | Some("remove") => {
                self.dialog = Dialog::Error(command_error(code, stderr));
            }
            Some("projects") => {
                if let Dialog::Goto(picker) = &mut self.dialog {
                    picker.targets.extend(parse_projects(&String::from_utf8_lossy(stdout)));
                }
            }
            Some("open") => match parse_info(&String::from_utf8_lossy(stdout)) {
                Some(info) if ok => {
                    self.open_session(&info.session_name(self.separator), &info.root);
                    close_self();
                }
                _ => self.dialog = Dialog::Error(command_error(code, stderr)),
            },
            _ => {}
        }
    }

    /// The go-to picker: running sessions right away, repositories once the
    /// scan of the project roots is done.
    fn start_goto(&mut self) {
        let sessions = self.session_summaries().into_iter().map(|(name, _)| Target::Session(name));
        self.dialog = Dialog::Goto(Picker { targets: sessions.collect(), ..Default::default() });
        let mut args = vec!["projects", self.project_depth.as_str()];
        args.extend(self.project_roots.split(':').filter(|r| !r.is_empty()));
        let context = BTreeMap::from([("op".to_string(), "projects".to_string())]);
        run_ws(&args, "/", context);
    }

    /// Switch to the workspace's session, or start it in `dir`.
    fn open_session(&self, name: &str, dir: &str) {
        if self.session.as_deref() == Some(name) {
            return;
        }
        if self.is_live(name) {
            switch_session(Some(name));
        } else {
            switch_session_with_cwd(Some(name), Some(PathBuf::from(dir)));
        }
    }

    /// End the session of a removed workspace. The own session is left
    /// first, for another session or else a new one in the main checkout
    /// `main`; the dialog goes with it.
    fn end_session(&mut self, name: &str, main: &str) {
        if self.session.as_deref() == Some(name) {
            self.leave_session(Some(main));
            return;
        }
        if self.is_live(name) {
            kill_sessions(&[name]);
        }
        close_self();
    }

    fn dialog_key(&mut self, key: KeyWithModifier) -> bool {
        let plain = key.key_modifiers.is_empty()
            || (key.key_modifiers.len() == 1 && key.key_modifiers.contains(&KeyModifier::Shift));
        self.dialog = match std::mem::take(&mut self.dialog) {
            Dialog::Prompt { info, mut prompt } => {
                match key.bare_key {
                    BareKey::Esc => {
                        close_self();
                        return false;
                    }
                    BareKey::Enter => match prompt.choice() {
                        Choice::Existing(e) => {
                            self.open_session(
                                &session_name(info.project(), self.separator, &e.name),
                                &e.root,
                            );
                            close_self();
                            return false;
                        }
                        Choice::New(name) => {
                            let slug = slug(&name).unwrap_or_default();
                            let dir = workspace_dir(&info.main, &slug);
                            let session = session_name(info.project(), self.separator, &slug);
                            let context = BTreeMap::from([
                                ("op".to_string(), "add".to_string()),
                                ("dir".to_string(), dir.clone()),
                                ("session".to_string(), session.clone()),
                            ]);
                            run_ws(&["add", &info.vcs, &dir, &name], &info.root, context);
                            self.dialog = Dialog::Running(format!("creating {}…", session));
                            return true;
                        }
                        Choice::Nothing => {}
                    },
                    BareKey::Up => prompt.move_selection(-1),
                    BareKey::Down => prompt.move_selection(1),
                    BareKey::Tab => prompt.complete(),
                    BareKey::Backspace => prompt.backspace(),
                    BareKey::Char(c) if plain && !c.is_control() => prompt.type_char(c),
                    _ => {}
                }
                Dialog::Prompt { info, prompt }
            }
            Dialog::Confirm { info } => match key.bare_key {
                BareKey::Char('y') | BareKey::Char('Y') => {
                    let session = info.session_name(self.separator);
                    let context = BTreeMap::from([
                        ("op".to_string(), "remove".to_string()),
                        ("session".to_string(), session.clone()),
                        ("main".to_string(), info.main.clone()),
                    ]);
                    // run from the main checkout: the workspace directory is deleted
                    run_ws(&["remove", &info.vcs, &info.root, &info.name], &info.main, context);
                    Dialog::Running(format!("removing {}…", session))
                }
                _ => {
                    close_self();
                    return false;
                }
            },
            Dialog::Goto(mut picker) => {
                let ctrl = key.key_modifiers.contains(&KeyModifier::Ctrl);
                match key.bare_key {
                    BareKey::Esc => {
                        close_self();
                        return false;
                    }
                    BareKey::Enter => match picker.choice() {
                        Some(Target::Session(name)) => {
                            if self.session.as_deref() != Some(name.as_str()) {
                                switch_session(Some(&name));
                            }
                            close_self();
                            return false;
                        }
                        Some(Target::Project { label, path }) => {
                            let context = BTreeMap::from([("op".to_string(), "open".to_string())]);
                            run_ws(&["info"], &path, context);
                            self.dialog = Dialog::Running(format!("opening {}…", label));
                            return true;
                        }
                        None => {}
                    },
                    BareKey::Up => picker.move_selection(-1),
                    BareKey::Down => picker.move_selection(1),
                    BareKey::Char('p') if ctrl => picker.move_selection(-1),
                    BareKey::Char('n') if ctrl => picker.move_selection(1),
                    BareKey::PageUp => picker.move_selection(-10),
                    BareKey::PageDown => picker.move_selection(10),
                    BareKey::Backspace => picker.backspace(),
                    BareKey::Char(c) if plain && !c.is_control() => picker.type_char(c),
                    _ => {}
                }
                Dialog::Goto(picker)
            }
            Dialog::Error(_) => {
                close_self();
                return false;
            }
            other => other,
        };
        true
    }

    fn render_dialog(&self, rows: usize, cols: usize) {
        let text = match &self.dialog {
            Dialog::Probing => "…".to_string(),
            Dialog::Prompt { info, prompt } => {
                let mut text = format!(
                    "Workspace for {}\n\nname: {}▏\n",
                    info.project(),
                    prompt.input
                );
                let matches = prompt.matches();
                if !matches.is_empty() {
                    text.push_str("\n\u{1b}[2mexisting:\u{1b}[0m\n");
                }
                for (i, e) in matches.iter().enumerate() {
                    let session = session_name(info.project(), self.separator, &e.name);
                    let open = if self.is_live(&session) { "  \u{1b}[2m(open)\u{1b}[0m" } else { "" };
                    if prompt.selected == Some(i) {
                        text.push_str(&format!("\u{1b}[7m› {}\u{1b}[0m{}\n", e.name, open));
                    } else {
                        text.push_str(&format!("  {}{}\n", e.name, open));
                    }
                }
                text.push_str(
                    "\n\u{1b}[2mEnter create or open · ↑↓ pick · Tab complete · Esc cancel\u{1b}[0m",
                );
                text
            }
            Dialog::Confirm { info } => format!(
                "Remove workspace {}?\n{}\n\n\u{1b}[2m{}\ny remove · any other key cancels\u{1b}[0m",
                info.session_name(self.separator),
                info.root,
                if info.vcs == "jj" {
                    "jj: its work stays in the repo, under a bookmark of the same name"
                } else {
                    "git: the branch stays; refuses if the worktree has changes"
                }
            ),
            Dialog::Goto(picker) => self.goto_text(picker, rows),
            Dialog::Running(msg) => msg.clone(),
            Dialog::Error(msg) => format!(
                "\u{1b}[31m{}\u{1b}[0m\n\n\u{1b}[2many key closes\u{1b}[0m",
                msg
            ),
        };
        let pad = " ".repeat(LEFT_PAD);
        let mut out = "\r\n".repeat(TOP_PAD);
        for line in text.lines() {
            out.push_str(&truncate_visible(&format!("{}{}", pad, line), cols));
            out.push_str("\r\n");
        }
        print!("{}", out);
    }

    /// The picker's query and the part of its matches around the selection
    /// that fits in `rows`.
    fn goto_text(&self, picker: &Picker, rows: usize) -> String {
        let mut text = format!("go to: {}▏\n\n", picker.input);
        let matches = picker.matches();
        // query, blank, list, blank, hint, and the line break after it
        let height = rows.saturating_sub(TOP_PAD + 5).max(1);
        let start = (picker.selected + 1).saturating_sub(height);
        let attention: BTreeMap<String, Option<Attention>> =
            self.session_summaries().into_iter().collect();
        for (i, target) in matches.iter().enumerate().skip(start).take(height) {
            let selected = i == picker.selected;
            let row = match target {
                Target::Session(name) => {
                    let dot = if self.session.as_deref() == Some(name.as_str()) { "●" } else { "▸" };
                    let icon = self.icon(attention.get(name).copied().flatten());
                    format!("{} {}{}", dot, icon, name)
                }
                Target::Project { label, .. } if selected => format!("  {}", label),
                Target::Project { label, .. } => format!("  \u{1b}[2m{}\u{1b}[0m", label),
            };
            if selected {
                text.push_str(&format!("\u{1b}[7m{}\u{1b}[0m\n", row));
            } else {
                text.push_str(&format!("{}\n", row));
            }
        }
        if matches.is_empty() {
            text.push_str("\u{1b}[2mno match\u{1b}[0m\n");
        }
        text.push_str(
            "\n\u{1b}[2mEnter open · ↑↓ / Ctrl+n Ctrl+p pick · Esc cancel · ▸ session, dim: repository\u{1b}[0m",
        );
        text
    }
}

#[cfg(not(test))]
impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.separator = configuration
            .get("separator")
            .and_then(|s| s.chars().next())
            .unwrap_or(':');
        self.waiting_icon = configuration
            .get("waiting_icon")
            .cloned()
            .unwrap_or_else(|| "◆".to_string());
        self.completed_icon = configuration
            .get("completed_icon")
            .cloned()
            .unwrap_or_else(|| "✓".to_string());
        self.spinner = parse_spinner(configuration.get("spinner"));
        self.new_key = configuration
            .get("new_key")
            .cloned()
            .unwrap_or_else(|| "Alt w".to_string());
        self.close_key = configuration
            .get("close_key")
            .cloned()
            .unwrap_or_else(|| "Alt W".to_string());
        self.goto_key = configuration
            .get("goto_key")
            .cloned()
            .unwrap_or_else(|| "Alt g".to_string());
        self.project_roots = configuration
            .get("project_roots")
            .cloned()
            .unwrap_or_else(|| "~".to_string());
        self.project_depth = configuration
            .get("project_depth")
            .filter(|d| d.parse::<u32>().is_ok())
            .cloned()
            .unwrap_or_else(|| "4".to_string());
        self.mode = match configuration.get("mode").map(String::as_str) {
            Some("new") => Mode::New,
            Some("close") => Mode::Close,
            Some("goto") => Mode::Goto,
            _ => Mode::Sidebar,
        };
        self.plugin_id = get_plugin_ids().plugin_id;
        // subscribe first: a reloaded plugin with permissions granted earlier
        // gets its PermissionRequestResult right away
        subscribe(&[
            EventType::ModeUpdate,
            EventType::TabUpdate,
            EventType::PaneUpdate,
            EventType::SessionUpdate,
            EventType::Key,
            EventType::Mouse,
            EventType::Timer,
            EventType::RunCommandResult,
            EventType::PermissionRequestResult,
        ]);
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::ReadCliPipes,
            PermissionType::RunCommands,
            PermissionType::Reconfigure,
        ]);
    }

    fn update(&mut self, event: Event) -> bool {
        if self.mode != Mode::Sidebar {
            return self.update_dialog(event);
        }
        // Tab and pane updates only arrive once the permissions are granted,
        // and a session started in the background never gets a
        // PermissionRequestResult (until a client attaches).
        if matches!(event, Event::TabUpdate(_) | Event::PaneUpdate(_)) {
            self.granted = true;
        }
        match event {
            Event::PermissionRequestResult(status) => {
                self.granted = status == PermissionStatus::Granted;
                self.bind_keys();
                self.probe_session_name();
                self.ensure_timer();
                false
            }
            Event::ModeUpdate(mode_info) => {
                if self.set_session_name(mode_info.session_name) {
                    self.sync_selection();
                    return true;
                }
                false
            }
            Event::SessionUpdate(sessions, _) => self.on_sessions(sessions),
            Event::TabUpdate(tabs) => {
                self.tabs = tabs;
                self.load_state();
                // Clear on focus: the active tab is "seen", so strip its marker.
                // Safe on every TabUpdate because set_attention never marks the
                // active tab of an attached session, so there's no marker here
                // to race with. An agent still waiting for input is marked again
                // by the next title read once you leave its tab without answering.
                self.clear_seen_tab();
                self.sync_selection();
                self.ensure_timer();
                true
            }
            Event::Timer(_) => {
                self.timer_running = false;
                if self.leave_tick() {
                    return false;
                }
                self.since_poll += self.timer_interval;
                let mut changed = false;
                if self.since_poll >= POLL_INTERVAL {
                    self.since_poll = 0.0;
                    self.poll_agents();
                    changed = self.refresh_sessions();
                }
                let working = self.any_working();
                if working {
                    self.spin = self.spin.wrapping_add(1);
                }
                self.ensure_timer();
                working || changed
            }
            Event::PaneUpdate(manifest) => {
                self.update_panes(&manifest);
                let titles = manifest
                    .panes
                    .iter()
                    .flat_map(|(&pos, panes)| {
                        panes.iter().filter(|p| !p.is_plugin).map(move |p| (pos, p.id, p.title.clone()))
                    })
                    .collect();
                self.track_agents(titles);
                self.bind_keys();
                self.probe_session_name();
                self.close_if_empty();
                false
            }
            Event::RunCommandResult(Some(0), stdout, _, context)
                if context.get("op").map(String::as_str) == Some("name") =>
            {
                if let Some(info) = parse_info(&String::from_utf8_lossy(&stdout)) {
                    self.rename_after_workspace(&info);
                }
                false
            }
            Event::Key(key) => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            _ => false,
        }
    }

    /// Attention signals: `zellij-workspaces::waiting|completed|working|clear-working::<pane_id>`
    /// (broadcast CLI pipe).
    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        if self.mode != Mode::Sidebar {
            return false;
        }
        let Some(rest) = pipe_message.name.strip_prefix("zellij-workspaces::") else {
            return false;
        };
        let Some((signal, pane_id)) = rest.split_once("::") else {
            return false;
        };
        let Ok(pane_id) = pane_id.parse::<u32>() else {
            return false;
        };
        if let Some(signal) = parse_signal(signal) {
            self.apply_signal(pane_id, signal);
        }
        false
    }

    fn render(&mut self, rows: usize, cols: usize) {
        if self.mode != Mode::Sidebar {
            self.render_dialog(rows, cols);
            return;
        }
        let visible = self.build_rows();
        let pad = " ".repeat(LEFT_PAD);
        let mut out = String::new();
        for _ in 0..TOP_PAD {
            out.push_str("\r\n");
        }
        if visible.is_empty() {
            out.push_str(&format!("{}\u{1b}[2m(no sessions)\u{1b}[0m", pad));
            print!("{}", out);
            return;
        }
        for (i, row) in visible.iter().enumerate() {
            let core = match row {
                Row::Group { name, collapsed, count, attention } => {
                    let disc = if *collapsed { "▶" } else { "▼" };
                    let editing = match &self.renaming {
                        Some((RenameTarget::Group(o), buf)) if o == name => Some(buf),
                        _ => None,
                    };
                    if let Some(buf) = editing {
                        format!("{} {}▏", disc, buf)
                    } else {
                        let icon = if *collapsed { self.icon(*attention) } else { String::new() };
                        format!("{} {}{} ({})", disc, icon, name, count)
                    }
                }
                Row::Session { session, label, current, attention } => {
                    let dot = if *current { "●" } else { " " };
                    let editing = match &self.renaming {
                        Some((RenameTarget::Session(s), buf)) if s == session => Some(buf),
                        _ => None,
                    };
                    if let Some(buf) = editing {
                        format!("  {} {}▏", dot, buf)
                    } else {
                        format!("  {} {}{}", dot, self.icon(*attention), label)
                    }
                }
            };
            let line = truncate_visible(&format!("{}{}", pad, core), cols);
            if i == self.selected {
                let w = visible_width(&line);
                let bar = if w < cols {
                    format!("{}{}", line, " ".repeat(cols - w))
                } else {
                    line
                };
                out.push_str(&format!("\u{1b}[7m{}\u{1b}[0m", bar));
            } else {
                out.push_str(&line);
            }
            out.push_str("\r\n");
        }
        print!("{}", out);
    }
}

/// Number of *visible* characters, ignoring ANSI CSI escape sequences (`ESC [ … letter`).
fn visible_width(s: &str) -> usize {
    let mut w = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // consume the escape sequence up to and including its final letter
            for e in chars.by_ref() {
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            w += 1;
        }
    }
    w
}

/// Truncate to `max` *visible* columns, preserving ANSI escapes intact (never cut
/// mid-sequence) and appending `…` when content is dropped.
fn truncate_visible(s: &str, max: usize) -> String {
    if visible_width(s) <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out = String::new();
    let mut w = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            out.push(c);
            for e in chars.by_ref() {
                out.push(e);
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            if w >= keep {
                break;
            }
            out.push(c);
            w += 1;
        }
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_attention_variants() {
        assert_eq!(parse_attention("work:api ⏳"), (Some(Attention::Waiting), "work:api"));
        assert_eq!(parse_attention("db ✅"), (Some(Attention::Completed), "db"));
        assert_eq!(parse_attention("run ⚙"), (Some(Attention::Working), "run"));
        assert_eq!(parse_attention("plain"), (None, "plain"));
    }

    #[test]
    fn markers_roundtrip_for_all_states() {
        for att in [Attention::Waiting, Attention::Working, Attention::Completed] {
            let name = format!("x:y{}", marker_of(att));
            assert_eq!(parse_attention(&name), (Some(att), "x:y"));
        }
    }

    #[test]
    fn parse_attention_roundtrips_with_set() {
        let (_, base) = parse_attention("x:y ⏳");
        assert_eq!(format!("{}{}", base, MARK_WAITING), "x:y ⏳");
    }

    #[test]
    fn group_of_splits_and_defaults() {
        let s = State { separator: ':', ..Default::default() };
        assert_eq!(s.group_of("work:api"), ("work".to_string(), "api"));
        assert_eq!(s.group_of("work: api"), ("work".to_string(), "api")); // trims one space
        assert_eq!(s.group_of("scratch"), ("General".to_string(), "scratch"));
    }

    #[test]
    fn merge_priority_waiting_working_completed() {
        assert_eq!(merge(Some(Attention::Completed), Attention::Waiting), Attention::Waiting);
        assert_eq!(merge(Some(Attention::Waiting), Attention::Completed), Attention::Waiting);
        assert_eq!(merge(Some(Attention::Working), Attention::Waiting), Attention::Waiting);
        assert_eq!(merge(Some(Attention::Completed), Attention::Working), Attention::Working);
        assert_eq!(merge(Some(Attention::Working), Attention::Completed), Attention::Working);
        assert_eq!(merge(None, Attention::Completed), Attention::Completed);
        assert_eq!(merge(None, Attention::Working), Attention::Working);
    }

    #[test]
    fn state_roundtrips_through_serialize_and_parse() {
        let p = Persisted {
            order: vec!["work".to_string(), "General".to_string()],
            collapsed: ["scratch".to_string()].into(),
            session_order: [(
                "work".to_string(),
                vec!["db".to_string(), "api".to_string()],
            )]
            .into(),
        };
        assert_eq!(parse_state(&serialize_state(&p)), p);
        assert_eq!(parse_state(""), Persisted::default());
        assert_eq!(parse_state("junk line\n"), Persisted::default());
    }

    #[test]
    fn sort_by_saved_orders_known_labels_then_native() {
        let item = |label: &str| Item {
            session: format!("p:{}", label),
            label: label.to_string(),
            current: false,
            attention: None,
        };
        let mut items = vec![item("a"), item("b"), item("c"), item("d")];
        sort_by_saved(&mut items, &["c".to_string(), "b".to_string()]);
        let got: Vec<&str> = items.iter().map(|it| it.label.as_str()).collect();
        // saved first, unseen (a, d) after in their native order
        assert_eq!(got, vec!["c", "b", "a", "d"]);
    }

    #[test]
    fn merge_order_saved_first_then_new_dropping_stale() {
        let saved = vec!["b".to_string(), "gone".to_string(), "a".to_string()];
        let appearance = vec!["a".to_string(), "b".to_string(), "new".to_string()];
        assert_eq!(merge_order(&saved, appearance), vec!["b", "a", "new"]);
        assert_eq!(merge_order(&[], vec!["x".to_string()]), vec!["x"]);
    }

    #[test]
    fn move_in_swaps_neighbors_and_respects_edges() {
        let mut order: Vec<String> =
            vec!["a".into(), "b".into(), "c".into()];
        assert!(move_in(&mut order, "b", 1));
        assert_eq!(order, vec!["a", "c", "b"]);
        assert!(!move_in(&mut order, "a", -1)); // already first
        assert!(!move_in(&mut order, "b", 1)); // already last
        assert!(!move_in(&mut order, "missing", 1));
        assert_eq!(order, vec!["a", "c", "b"]); // edges/missing leave order untouched
    }

    #[test]
    fn empty_tab_closes_or_leaves_only_after_its_panes_are_gone() {
        // a fresh tab may list only the sidebar before its terminal exists
        assert_eq!(empty_tab_action(false, 0, 3), None);
        assert_eq!(empty_tab_action(true, 1, 3), None);
        assert_eq!(empty_tab_action(true, 0, 3), Some(EmptyTab::Close));
        assert_eq!(empty_tab_action(true, 0, 1), Some(EmptyTab::Leave));
    }

    #[test]
    fn omp_titles_parsed_into_agent_states() {
        assert_eq!(parse_agent_title("π ⠙ Add agent skill"), Some(AgentTitle::Working));
        assert_eq!(parse_agent_title("π : static host"), Some(AgentTitle::Working));
        assert_eq!(parse_agent_title("π ! Announce events"), Some(AgentTitle::Attention));
        assert_eq!(parse_agent_title("π > Improve alerts"), Some(AgentTitle::Idle));
        assert_eq!(parse_agent_title("π >"), Some(AgentTitle::Idle)); // no session title yet
        // titleState disabled, other programs
        assert_eq!(parse_agent_title("π: Improve alerts"), None);
        assert_eq!(parse_agent_title("π"), None);
        assert_eq!(parse_agent_title("π pi.rs"), None);
        assert_eq!(parse_agent_title("Pane #2"), None);
    }

    #[test]
    fn agent_transitions_map_to_markers() {
        use AgentTitle::*;
        assert_eq!(agent_signal(None, Some(Working)), Some(Signal::Working));
        assert_eq!(agent_signal(Some(Working), Some(Attention)), Some(Signal::Waiting));
        assert_eq!(agent_signal(Some(Working), Some(Idle)), Some(Signal::Completed));
        // nothing finished: started idle, or stopped after asking
        assert_eq!(agent_signal(None, Some(Idle)), None);
        assert_eq!(agent_signal(Some(Attention), Some(Idle)), Some(Signal::ClearWorking));
        // agent gone (exited or title replaced)
        assert_eq!(agent_signal(Some(Working), None), Some(Signal::ClearWorking));
        assert_eq!(agent_signal(Some(Working), Some(Working)), None);
    }

    #[test]
    fn generated_session_names_detected() {
        assert!(is_generated_session_name("gregarious-cactus"));
        assert!(!is_generated_session_name("proj:main"));
        assert!(!is_generated_session_name("work"));
        assert!(!is_generated_session_name("my-fancy-session"));
        assert!(!is_generated_session_name("Big-Cat"));
        assert!(!is_generated_session_name("-cactus"));
    }

    #[test]
    fn leaving_prefers_another_session_of_the_same_project() {
        let live = |name: &str| SessionInfo { name: name.to_string(), ..Default::default() };
        let mut s = State {
            separator: ':',
            session: Some("proj:feat".to_string()),
            sessions: vec![live("aaa:main"), live("proj:feat"), live("proj:main")],
            ..Default::default()
        };
        assert_eq!(s.fallback_session("proj:feat").as_deref(), Some("proj:main"));
        // no other session of the project: the first one in sidebar order
        s.sessions.retain(|x| x.name != "proj:main");
        assert_eq!(s.fallback_session("proj:feat").as_deref(), Some("aaa:main"));
        s.sessions.retain(|x| x.name == "proj:feat");
        assert_eq!(s.fallback_session("proj:feat"), None);
    }

    #[test]
    fn parse_spinner_splits_chars_with_default_fallback() {
        assert_eq!(parse_spinner(Some(&"◐◓◑◒".to_string())), vec!["◐", "◓", "◑", "◒"]);
        let default: Vec<String> = SPINNER_DEFAULT.chars().map(String::from).collect();
        assert_eq!(parse_spinner(None), default);
        assert_eq!(parse_spinner(Some(&"  ".to_string())), default); // whitespace-only
    }

    #[test]
    fn migrate_group_state_moves_all_entries() {
        let mut s = State {
            group_order: vec!["work".to_string(), "misc".to_string()],
            collapsed: ["work".to_string()].into(),
            session_order: [("work".to_string(), vec!["api".to_string()])].into(),
            ..Default::default()
        };
        s.migrate_group_state("work", "proj");
        assert_eq!(s.group_order, vec!["proj", "misc"]);
        assert!(s.collapsed.contains("proj") && !s.collapsed.contains("work"));
        assert_eq!(s.session_order.get("proj"), Some(&vec!["api".to_string()]));
        assert!(!s.session_order.contains_key("work"));
        // renaming into an existing group must not duplicate it in the order
        s.migrate_group_state("proj", "misc");
        assert_eq!(s.group_order, vec!["misc"]);
    }

    #[test]
    fn rename_label_first_match_only() {
        let mut order = vec!["a".to_string(), "b".to_string(), "a".to_string()];
        rename_label(&mut order, "a", "z");
        assert_eq!(order, vec!["z", "b", "a"]);
        rename_label(&mut order, "missing", "x");
        assert_eq!(order, vec!["z", "b", "a"]);
    }

    #[test]
    fn visible_width_ignores_ansi() {
        // ◆, space, x  => 3 visible; the color codes count for nothing
        assert_eq!(visible_width("\u{1b}[33m◆\u{1b}[39m x"), 3);
        assert_eq!(visible_width("plain"), 5);
    }

    #[test]
    fn truncate_visible_keeps_escapes_and_width() {
        let s = "\u{1b}[33m◆\u{1b}[39m hello"; // visible "◆ hello" = 7
        let out = truncate_visible(s, 4);
        assert_eq!(visible_width(&out), 4); // 3 kept + …
        assert!(out.contains("\u{1b}[33m")); // escape preserved, not sliced
        assert_eq!(truncate_visible("abc", 5), "abc"); // no-op when it fits
    }
}
