// In `cargo test` the plugin entry points are gated out (see below), so the
// methods they call look unused — silence that only for test builds.
#![cfg_attr(test, allow(dead_code))]

mod workspace;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use workspace::{
    keybind_kdl, parse_info, parse_list, run_ws, slug, workspace_dir, Choice, Dialog, Prompt,
    WsInfo, NOT_A_REPO,
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

/// Group order + collapse state, shared across the per-tab plugin instances and
/// across restarts. `/cache` is mounted per plugin *location* (host side:
/// `~/.cache/zellij/<location>/plugin_cache`), so every instance sees the same
/// files; each re-reads its file on `TabUpdate`, which fires on every tab switch.
/// One file per *session* (suffix = session name, learned from `ModeUpdate`) —
/// different sessions have different groups, and a single shared file would let
/// each session's save wipe the others' state.
const STATE_FILE_PREFIX: &str = "/cache/state-";

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

/// Zellij's default names for unnamed tabs ("Tab #1", …): such a tab gets its
/// workspace name probed once, without waiting for a cwd change.
fn is_default_tab_name(name: &str) -> bool {
    name.strip_prefix("Tab #")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

#[derive(Debug, PartialEq, Eq)]
enum EmptyTab {
    Close,
    Quit,
}

/// What to do about the sidebar's own tab, given whether it ever had other
/// panes, how many it has now, and how many tabs the session has.
fn empty_tab_action(had_content: bool, others: usize, tabs: usize) -> Option<EmptyTab> {
    if !had_content || others > 0 {
        None
    } else if tabs <= 1 {
        Some(EmptyTab::Quit)
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

/// Persisted sidebar state: group display order, collapsed groups, and — only
/// for groups the user explicitly reordered tabs in — per-group tab label order.
/// Groups absent from `tab_order` keep following Zellij's native tab positions.
#[derive(Default, PartialEq, Debug)]
struct Persisted {
    order: Vec<String>,
    collapsed: BTreeSet<String>,
    tab_order: BTreeMap<String, Vec<String>>,
}

/// One line per entry: `order <group>` / `collapsed <group>` /
/// `taborder <group>\t<label>` (labels of one group in order, one per line).
/// Names may contain anything but a newline (and, for groups, a tab).
/// Unknown lines are ignored.
fn parse_state(s: &str) -> Persisted {
    let mut p = Persisted::default();
    for line in s.lines() {
        if let Some(g) = line.strip_prefix("order ") {
            p.order.push(g.to_string());
        } else if let Some(g) = line.strip_prefix("collapsed ") {
            p.collapsed.insert(g.to_string());
        } else if let Some(rest) = line.strip_prefix("taborder ") {
            if let Some((g, label)) = rest.split_once('\t') {
                p.tab_order
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
    for (g, labels) in &p.tab_order {
        for label in labels {
            out.push_str("taborder ");
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
fn sort_by_saved(items: &mut [TabItem], saved: &[String]) {
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
    tabs: Vec<TabInfo>,
    /// terminal pane id -> tab position (rebuilt on each PaneUpdate)
    pane_tab: BTreeMap<u32, usize>,
    collapsed: BTreeSet<String>,
    /// saved group display order; groups not listed here follow in first-appearance order
    group_order: Vec<String>,
    /// per-group saved tab label order; groups not present follow native tab positions
    tab_order: BTreeMap<String, Vec<String>>,
    /// session name (from ModeUpdate) — state persistence is a no-op until known
    session: Option<String>,
    selected: usize,
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
    /// close/quit already requested for the own tab
    closing: bool,
    granted: bool,
    keys_bound: bool,
    new_key: String,
    close_key: String,
    /// tab id whose default "Tab #N" name was already probed
    probed_tab: Option<usize>,
    dialog: Dialog,
    /// active inline rename edit: (target, input buffer)
    renaming: Option<(RenameTarget, String)>,
    /// animation frames (from the `spinner` config key), frame counter, and
    /// whether a Timer event is already scheduled
    spinner: Vec<String>,
    spin: usize,
    timer_running: bool,
}

enum RenameTarget {
    Group(String),
    /// tab position; edits the tab's label (group prefix is kept)
    Tab(usize),
}

// The plugin entry points call zellij-tile's wasm host imports, which don't exist
// on the host target — gate them out of `cargo test` so the pure logic can be tested.
#[cfg(not(test))]
register_plugin!(State);

/// A tab within a group, while building the tree.
struct TabItem {
    position: usize,
    label: String,
    active: bool,
    attention: Option<Attention>,
}

enum Row {
    Group { name: String, collapsed: bool, count: usize, attention: Option<Attention> },
    Tab { position: usize, label: String, active: bool, attention: Option<Attention> },
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

    /// Groups in first-appearance (tab position) order.
    fn appearance_groups(&self) -> Vec<String> {
        let mut out = Vec::new();
        for t in &self.tabs {
            let (_, base) = parse_attention(&t.name);
            let (g, _) = self.group_of(base);
            if !out.contains(&g) {
                out.push(g);
            }
        }
        out
    }

    fn display_group_order(&self) -> Vec<String> {
        merge_order(&self.group_order, self.appearance_groups())
    }

    fn state_file(&self) -> Option<String> {
        self.session
            .as_ref()
            .map(|s| format!("{}{}", STATE_FILE_PREFIX, s))
    }

    fn load_state(&mut self) {
        let Some(path) = self.state_file() else {
            return;
        };
        if let Ok(s) = std::fs::read_to_string(path) {
            let p = parse_state(&s);
            self.group_order = p.order;
            self.collapsed = p.collapsed;
            self.tab_order = p.tab_order;
        }
    }

    fn save_state(&self) {
        let Some(path) = self.state_file() else {
            return;
        };
        let p = Persisted {
            order: self.display_group_order(),
            collapsed: self.collapsed.clone(),
            tab_order: self.tab_order.clone(),
        };
        let _ = std::fs::write(path, serialize_state(&p));
    }

    /// Groups in display order, each with its tabs in display order.
    fn grouped_items(&self) -> Vec<(String, Vec<TabItem>)> {
        let mut groups: BTreeMap<String, Vec<TabItem>> = BTreeMap::new();
        for t in &self.tabs {
            let (attention, base) = parse_attention(&t.name);
            let (g, label) = self.group_of(base);
            groups.entry(g).or_default().push(TabItem {
                position: t.position,
                label: label.to_string(),
                active: t.active,
                attention,
            });
        }
        self.display_group_order()
            .iter()
            .filter_map(|g| {
                let mut items = groups.remove(g)?;
                if let Some(saved) = self.tab_order.get(g) {
                    sort_by_saved(&mut items, saved);
                }
                Some((g.clone(), items))
            })
            .collect()
    }

    fn build_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (g, items) in self.grouped_items() {
            let g = &g;
            let collapsed = self.collapsed.contains(g);
            let group_att = items.iter().fold(None, |acc, it| match it.attention {
                Some(x) => Some(merge(acc, x)),
                None => acc,
            });
            rows.push(Row::Group {
                name: g.clone(),
                collapsed,
                count: items.len(),
                attention: group_att,
            });
            if !collapsed {
                for it in items {
                    rows.push(Row::Tab {
                        position: it.position,
                        label: it.label,
                        active: it.active,
                        attention: it.attention,
                    });
                }
            }
        }
        rows
    }

    fn active_row_index(&self) -> Option<usize> {
        self.build_rows()
            .iter()
            .position(|r| matches!(r, Row::Tab { active: true, .. }))
    }

    fn icon(&self, att: Option<Attention>) -> String {
        match att {
            Some(Attention::Waiting) => format!("\u{1b}[33m{}\u{1b}[39m ", self.waiting_icon),
            Some(Attention::Working) => {
                let frame = self
                    .spinner
                    .get(self.spin % self.spinner.len().max(1))
                    .map(String::as_str)
                    .unwrap_or("⣾");
                format!("\u{1b}[36m{}\u{1b}[39m ", frame)
            }
            Some(Attention::Completed) => format!("\u{1b}[32m{}\u{1b}[39m ", self.completed_icon),
            None => String::new(),
        }
    }

    fn any_working(&self) -> bool {
        self.tabs
            .iter()
            .any(|t| parse_attention(&t.name).0 == Some(Attention::Working))
    }

    /// Arm the animation timer if something is working and none is scheduled.
    fn ensure_timer(&mut self) {
        if self.any_working() && !self.timer_running {
            set_timeout(SPINNER_INTERVAL);
            self.timer_running = true;
        }
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
    fn set_attention(&self, pane_id: u32, attention: Attention) {
        let Some(t) = self.fresh_tab(pane_id) else {
            return;
        };
        let (att, base) = parse_attention(&t.name);
        // Never mark the tab you're already looking at — you don't need an
        // attention cue for it, and it keeps set/clear free of any race.
        // But a working spinner on it still has to END now.
        if t.active {
            if att == Some(Attention::Working) {
                rename_tab_with_id(t.tab_id as u64, base.to_string());
            }
            return;
        }
        if att == Some(attention) {
            return;
        }
        rename_tab_with_id(t.tab_id as u64, format!("{}{}", base, marker_of(attention)));
    }

    /// Mark the tab containing `pane_id` as working. Unlike attention marks
    /// this applies to the active tab too (it is not cleared on focus, so
    /// there is no set/clear race — only pipes ever end it).
    fn set_working(&self, pane_id: u32) {
        if let Some(t) = self.fresh_tab(pane_id) {
            let (att, base) = parse_attention(&t.name);
            if att != Some(Attention::Working) {
                rename_tab_with_id(t.tab_id as u64, format!("{}{}", base, MARK_WORKING));
            }
        }
    }

    /// Strip a working marker (Claude exited without a Stop) — attention marks stay.
    fn clear_working(&self, pane_id: u32) {
        if let Some(t) = self.fresh_tab(pane_id) {
            let (att, base) = parse_attention(&t.name);
            if att == Some(Attention::Working) {
                rename_tab_with_id(t.tab_id as u64, base.to_string());
            }
        }
    }

    /// Ask the host which workspace `cwd` belongs to; the answer renames the
    /// tab of `pane_id` (see `rename_for_workspace`).
    fn probe_name(&self, pane_id: u32, cwd: &Path) {
        let context = BTreeMap::from([
            ("op".to_string(), "name".to_string()),
            ("pane".to_string(), pane_id.to_string()),
        ]);
        run_ws(&["info"], &cwd.to_string_lossy(), context);
    }

    /// Name the tab containing `pane_id` `project:workspace`, keeping its
    /// attention marker.
    fn rename_for_workspace(&self, pane_id: u32, info: &WsInfo) {
        let Some(t) = self.fresh_tab(pane_id) else {
            return;
        };
        let (att, base) = parse_attention(&t.name);
        let name = info.tab_name(self.separator);
        if base != name {
            let marker = att.map(marker_of).unwrap_or("");
            rename_tab_with_id(t.tab_id as u64, format!("{}{}", name, marker));
        }
    }

    /// Shells only report a cwd *change*, so a tab whose shell starts inside a
    /// repository (first tab, `Ctrl t n`) still has its default name: probe its
    /// first pane's cwd once.
    fn probe_default_tab(&mut self) {
        if !self.granted {
            return;
        }
        let Some(tab) = self
            .own_tab
            .and_then(|pos| self.tabs.iter().find(|t| t.position == pos))
        else {
            return;
        };
        if self.probed_tab == Some(tab.tab_id) || !is_default_tab_name(parse_attention(&tab.name).1)
        {
            return;
        }
        let Some(&pane_id) = self.own_terminal_panes.first() else {
            return;
        };
        self.probed_tab = Some(tab.tab_id);
        if let Ok(cwd) = get_pane_cwd(PaneId::Terminal(pane_id)) {
            self.probe_name(pane_id, &cwd);
        }
    }

    /// Register the create/remove keybindings (in memory, not written to the
    /// user's config). Every sidebar instance binds the same keys to the same
    /// action, so repeating this per tab is harmless.
    fn bind_keys(&mut self) {
        if self.keys_bound || !self.granted {
            return;
        }
        let Some(url) = &self.own_url else {
            return;
        };
        if let Some(kdl) = keybind_kdl(url, &self.new_key, &self.close_key) {
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
        // (`zellij:link` sits suppressed in the first tab). Suppressed
        // terminals do count: zellij restores them when their replacement closes.
        self.own_tab_others = own_panes
            .map(|panes| {
                panes
                    .iter()
                    .filter(|p| {
                        p.is_selectable
                            && !(p.is_plugin && (p.id == self.plugin_id || p.is_suppressed))
                    })
                    .count()
            })
            .unwrap_or(0);
        if self.own_tab_others > 0 {
            self.own_tab_had_content = true;
        }
    }

    /// The sidebar keeps its tab alive after the last real pane is gone, which
    /// zellij would otherwise close: close the tab, or end the session when it
    /// is the last one, like zellij does when the last pane exits. The
    /// workspace on disk is untouched; Alt w with its name reopens it.
    fn close_if_empty(&mut self) {
        if self.closing {
            return;
        }
        match empty_tab_action(self.own_tab_had_content, self.own_tab_others, self.tabs.len()) {
            Some(EmptyTab::Quit) => {
                if let Some(path) = self.state_file() {
                    let _ = std::fs::remove_file(path);
                }
                self.closing = true;
                quit_zellij();
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

    /// Strip a *seen* attention marker from the tab at `pos` (global rename).
    /// A working spinner survives focus — peeking at a running task must not
    /// kill its indicator; only waiting/completed are "acknowledged by looking".
    fn clear_tab(&self, pos: usize) {
        if let Some(t) = self.tabs.iter().find(|t| t.position == pos) {
            let (att, base) = parse_attention(&t.name);
            if matches!(att, Some(Attention::Waiting) | Some(Attention::Completed)) {
                rename_tab_with_id(t.tab_id as u64, base.to_string());
            }
        }
    }

    fn activate(&mut self, idx: usize) -> bool {
        let rows = self.build_rows();
        if idx >= rows.len() {
            return false;
        }
        match &rows[idx] {
            Row::Group { name, .. } => {
                if self.collapsed.contains(name) {
                    self.collapsed.remove(name);
                } else {
                    self.collapsed.insert(name.clone());
                }
                self.save_state();
                true
            }
            Row::Tab { position, .. } => {
                switch_tab_to(*position as u32 + 1);
                true
            }
        }
    }

    /// Move the row under the selection up/down (persisted): a group header
    /// reorders the group, a tab row reorders the tab within its group.
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
            Some(Row::Tab { position, label, .. }) => {
                let (position, label) = (*position, label.clone());
                let Some((group, items)) = self
                    .grouped_items()
                    .into_iter()
                    .find(|(_, items)| items.iter().any(|it| it.position == position))
                else {
                    return false;
                };
                let mut labels: Vec<String> =
                    items.into_iter().map(|it| it.label).collect();
                if !move_in(&mut labels, &label, delta) {
                    return false;
                }
                let present = self.appearance_groups();
                self.tab_order.retain(|g, _| present.contains(g));
                self.tab_order.insert(group, labels);
                self.save_state();
                // keep the selection on the tab we just moved
                if let Some(idx) = self
                    .build_rows()
                    .iter()
                    .position(|r| matches!(r, Row::Tab { position: p, .. } if *p == position))
                {
                    self.selected = idx;
                }
                true
            }
            None => false,
        }
    }

    /// Move `old` group's saved order/collapse/tab-order entries to `new`.
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
        if let Some(order) = self.tab_order.remove(old) {
            self.tab_order.entry(new.to_string()).or_insert(order);
        }
    }

    /// Rename group `old` to `new`: re-prefix every member tab (preserving
    /// attention marks) and migrate the group's saved state.
    fn commit_rename(&mut self, old: &str, new: &str) {
        let new = new.trim();
        if new.is_empty() || new == old {
            return;
        }
        for t in &self.tabs {
            let (att, base) = parse_attention(&t.name);
            let (g, label) = self.group_of(base);
            if g != old {
                continue;
            }
            let marker = att.map(marker_of).unwrap_or("");
            rename_tab(
                t.position as u32 + 1,
                format!("{}{}{}{}", new, self.separator, label, marker),
            );
        }
        self.migrate_group_state(old, new);
        self.save_state();
    }

    /// Rename the label of the tab at `position`, keeping its group prefix
    /// (ungrouped tabs stay ungrouped) and attention mark.
    fn commit_tab_rename(&mut self, position: usize, new_label: &str) {
        let new_label = new_label.trim();
        let Some(t) = self.tabs.iter().find(|t| t.position == position) else {
            return;
        };
        let (att, base) = parse_attention(&t.name);
        let (g, old_label) = self.group_of(base);
        if new_label.is_empty() || new_label == old_label {
            return;
        }
        let marker = att.map(marker_of).unwrap_or("");
        let name = if base.find(self.separator).is_none() {
            new_label.to_string()
        } else {
            format!("{}{}{}", g, self.separator, new_label)
        };
        rename_tab(position as u32 + 1, format!("{}{}", name, marker));
        if let Some(order) = self.tab_order.get_mut(&g) {
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
                    RenameTarget::Tab(pos) => self.commit_tab_rename(*pos, &buf),
                }
                return true;
            }
            BareKey::Esc => return true, // buffer dropped = cancelled
            BareKey::Backspace => {
                buf.pop();
            }
            // separator/tab would corrupt group parsing / the state file format
            BareKey::Char(c) if plain && !c.is_control() && c != self.separator => {
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
                        self.renaming =
                            Some((RenameTarget::Group(name.clone()), name.clone()));
                        true
                    }
                    Some(Row::Tab { position, label, .. }) => {
                        self.renaming =
                            Some((RenameTarget::Tab(*position), label.clone()));
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
                        _ => "new workspace",
                    };
                    rename_plugin_pane(self.plugin_id, title);
                    let cwd = get_plugin_ids().initial_cwd;
                    let context = BTreeMap::from([("op".to_string(), "probe".to_string())]);
                    run_ws(&["info"], &cwd.to_string_lossy(), context);
                } else {
                    self.dialog = Dialog::Error("permissions were denied".to_string());
                }
                true
            }
            Event::TabUpdate(tabs) => {
                self.tabs = tabs;
                false
            }
            Event::PaneUpdate(manifest) => {
                self.update_panes(&manifest);
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
                            run_ws(&["list"], &info.root, context);
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
                if let (Some(tab), Some(dir)) = (context.get("tab"), context.get("dir")) {
                    self.open_tab(tab, dir);
                }
                close_self();
            }
            Some("remove") if ok => {
                if let Some(tab_id) = self.own_tab_id() {
                    close_tab_with_id(tab_id as u64);
                }
                close_self();
            }
            Some("add") | Some("remove") => {
                self.dialog = Dialog::Error(command_error(code, stderr));
            }
            _ => {}
        }
    }

    /// The tab already showing workspace tab name `name`, if any.
    fn tab_named(&self, name: &str) -> Option<&TabInfo> {
        self.tabs.iter().find(|t| parse_attention(&t.name).1 == name)
    }

    /// Switch to the workspace's tab, or open one in `dir`.
    fn open_tab(&self, name: &str, dir: &str) {
        match self.tab_named(name) {
            Some(t) => switch_tab_to(t.position as u32 + 1),
            None => {
                new_tab(Some(name), Some(dir));
            }
        }
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
                            let tab = format!("{}{}{}", info.project(), self.separator, e.name);
                            self.open_tab(&tab, &e.root);
                            close_self();
                            return false;
                        }
                        Choice::New(name) => {
                            let slug = slug(&name).unwrap_or_default();
                            let dir = workspace_dir(&info.main, &slug);
                            let tab = format!("{}{}{}", info.project(), self.separator, slug);
                            let context = BTreeMap::from([
                                ("op".to_string(), "add".to_string()),
                                ("dir".to_string(), dir.clone()),
                                ("tab".to_string(), tab.clone()),
                            ]);
                            run_ws(&["add", &info.vcs, &dir, &name], &info.root, context);
                            self.dialog = Dialog::Running(format!("creating {}…", tab));
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
                    let context = BTreeMap::from([("op".to_string(), "remove".to_string())]);
                    // run from the main checkout: the workspace directory is deleted
                    run_ws(&["remove", &info.vcs, &info.root, &info.name], &info.main, context);
                    Dialog::Running(format!("removing {}…", info.tab_name(self.separator)))
                }
                _ => {
                    close_self();
                    return false;
                }
            },
            Dialog::Error(_) => {
                close_self();
                return false;
            }
            other => other,
        };
        true
    }

    fn render_dialog(&self, cols: usize) {
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
                    let tab = format!("{}{}{}", info.project(), self.separator, e.name);
                    let open = if self.tab_named(&tab).is_some() { "  \u{1b}[2m(open)\u{1b}[0m" } else { "" };
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
                info.tab_name(self.separator),
                info.root,
                if info.vcs == "jj" {
                    "jj: its work stays in the repo, under a bookmark of the same name"
                } else {
                    "git: the branch stays; refuses if the worktree has changes"
                }
            ),
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
        self.mode = match configuration.get("mode").map(String::as_str) {
            Some("new") => Mode::New,
            Some("close") => Mode::Close,
            _ => Mode::Sidebar,
        };
        self.plugin_id = get_plugin_ids().plugin_id;
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::ReadCliPipes,
            PermissionType::RunCommands,
            PermissionType::Reconfigure,
        ]);
        subscribe(&[
            EventType::ModeUpdate,
            EventType::TabUpdate,
            EventType::PaneUpdate,
            EventType::Key,
            EventType::Mouse,
            EventType::Timer,
            EventType::CwdChanged,
            EventType::RunCommandResult,
            EventType::PermissionRequestResult,
        ]);
    }

    fn update(&mut self, event: Event) -> bool {
        if self.mode != Mode::Sidebar {
            return self.update_dialog(event);
        }
        match event {
            Event::PermissionRequestResult(status) => {
                self.granted = status == PermissionStatus::Granted;
                self.bind_keys();
                self.probe_default_tab();
                false
            }
            Event::ModeUpdate(mode_info) => {
                if mode_info.session_name != self.session {
                    self.session = mode_info.session_name;
                    self.load_state();
                    return true;
                }
                false
            }
            Event::TabUpdate(tabs) => {
                self.tabs = tabs;
                // Pick up order/collapse changes written by other instances —
                // every tab switch fires a TabUpdate, so the visible sidebar
                // is always freshly synced.
                self.load_state();
                // Clear on focus: the active tab is "seen", so strip its marker.
                // Safe on every TabUpdate because set_attention never marks the
                // active tab, so there's no marker here to race with.
                if let Some(pos) = self.tabs.iter().find(|t| t.active).map(|t| t.position) {
                    self.clear_tab(pos);
                }
                if let Some(i) = self.active_row_index() {
                    self.selected = i;
                } else {
                    let len = self.build_rows().len();
                    if len > 0 && self.selected >= len {
                        self.selected = len - 1;
                    }
                }
                self.ensure_timer();
                self.probe_default_tab();
                true
            }
            Event::Timer(_) => {
                self.timer_running = false;
                if self.any_working() {
                    self.spin = self.spin.wrapping_add(1);
                    self.ensure_timer();
                    true
                } else {
                    false
                }
            }
            Event::PaneUpdate(manifest) => {
                self.update_panes(&manifest);
                self.bind_keys();
                self.probe_default_tab();
                self.close_if_empty();
                false
            }
            // Every sidebar instance gets every cwd change; only the one in
            // the pane's own tab acts on it.
            Event::CwdChanged(PaneId::Terminal(pane_id), cwd, _) => {
                if self.granted
                    && self.own_tab.is_some()
                    && self.pane_tab.get(&pane_id) == self.own_tab.as_ref()
                {
                    self.probe_name(pane_id, &cwd);
                }
                false
            }
            Event::RunCommandResult(Some(0), stdout, _, context)
                if context.get("op").map(String::as_str) == Some("name") =>
            {
                let pane = context.get("pane").and_then(|p| p.parse::<u32>().ok());
                if let (Some(pane), Some(info)) =
                    (pane, parse_info(&String::from_utf8_lossy(&stdout)))
                {
                    self.rename_for_workspace(pane, &info);
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
        match signal {
            "waiting" => self.set_attention(pane_id, Attention::Waiting),
            "completed" => self.set_attention(pane_id, Attention::Completed),
            "working" => self.set_working(pane_id),
            "clear-working" => self.clear_working(pane_id),
            _ => {}
        }
        false
    }

    fn render(&mut self, _rows: usize, cols: usize) {
        if self.mode != Mode::Sidebar {
            self.render_dialog(cols);
            return;
        }
        let visible = self.build_rows();
        let pad = " ".repeat(LEFT_PAD);
        let mut out = String::new();
        for _ in 0..TOP_PAD {
            out.push_str("\r\n");
        }
        if visible.is_empty() {
            out.push_str(&format!("{}\u{1b}[2m(no tabs)\u{1b}[0m", pad));
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
                Row::Tab { position, label, active, attention } => {
                    let dot = if *active { "●" } else { " " };
                    let editing = match &self.renaming {
                        Some((RenameTarget::Tab(p), buf)) if p == position => Some(buf),
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
            tab_order: [(
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
        let item = |label: &str, position: usize| TabItem {
            position,
            label: label.to_string(),
            active: false,
            attention: None,
        };
        let mut items = vec![item("a", 0), item("b", 1), item("c", 2), item("b", 3)];
        sort_by_saved(&mut items, &["c".to_string(), "b".to_string()]);
        let got: Vec<(usize, &str)> =
            items.iter().map(|it| (it.position, it.label.as_str())).collect();
        // saved first (c, then both b's in stable native order), unseen (a) after
        assert_eq!(got, vec![(2, "c"), (1, "b"), (3, "b"), (0, "a")]);
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
    fn empty_tab_closes_or_quits_only_after_its_panes_are_gone() {
        // a fresh tab may list only the sidebar before its terminal exists
        assert_eq!(empty_tab_action(false, 0, 3), None);
        assert_eq!(empty_tab_action(true, 1, 3), None);
        assert_eq!(empty_tab_action(true, 0, 3), Some(EmptyTab::Close));
        assert_eq!(empty_tab_action(true, 0, 1), Some(EmptyTab::Quit));
    }

    #[test]
    fn default_tab_names_detected() {
        assert!(is_default_tab_name("Tab #1"));
        assert!(is_default_tab_name("Tab #42"));
        assert!(!is_default_tab_name("Tab #"));
        assert!(!is_default_tab_name("Tab #1x"));
        assert!(!is_default_tab_name("work:api"));
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
            tab_order: [("work".to_string(), vec!["api".to_string()])].into(),
            ..Default::default()
        };
        s.migrate_group_state("work", "proj");
        assert_eq!(s.group_order, vec!["proj", "misc"]);
        assert!(s.collapsed.contains("proj") && !s.collapsed.contains("work"));
        assert_eq!(s.tab_order.get("proj"), Some(&vec!["api".to_string()]));
        assert!(!s.tab_order.contains_key("work"));
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
