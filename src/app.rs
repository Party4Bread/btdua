//! Terminal-free UI state and key handling.

use std::collections::{BTreeSet, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::actions::Exact;
use crate::tree::{Counters, NodeId, ROOT, Tree};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Represented,
    Distributed,
    Exclusive,
    Shared,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Represented, Mode::Distributed, Mode::Exclusive, Mode::Shared];

    pub fn next(self) -> Mode {
        let i = Mode::ALL.iter().position(|&m| m == self).unwrap();
        Mode::ALL[(i + 1) % Mode::ALL.len()]
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Represented => "represented",
            Mode::Distributed => "distributed",
            Mode::Exclusive => "exclusive",
            Mode::Shared => "shared",
        }
    }

    pub fn value(self, c: &Counters) -> f64 {
        match self {
            Mode::Represented => c.represented as f64,
            Mode::Distributed => c.distributed,
            Mode::Exclusive => c.exclusive as f64,
            Mode::Shared => c.shared as f64,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortBy {
    Size,
    Name,
    Count,
}

#[derive(Debug)]
pub enum Dialog {
    None,
    Help,
    Info { node: NodeId },
    Confirm { targets: Vec<NodeId>, stage: u8 },
    Export { input: String },
    Message { title: String, text: String },
}

impl Dialog {
    pub fn message(title: impl Into<String>, text: impl Into<String>) -> Dialog {
        Dialog::Message { title: title.into(), text: text.into() }
    }
}

#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    Quit,
    TogglePause,
    Delete(Vec<NodeId>),
    Export(String),
    Exact(NodeId),
}

pub enum ExactState {
    Running,
    Done(Exact),
    Failed(String),
}

pub struct App {
    pub dir: NodeId,
    /// Per directory: selected child and its last index (fallback when it vanishes).
    selected: HashMap<NodeId, (NodeId, usize)>,
    pub mode: Mode,
    pub sort: SortBy,
    pub reverse: bool,
    pub marks: BTreeSet<NodeId>,
    pub dialog: Dialog,
    pub show_optional: bool,
    pub live: bool,
    pub paused: bool,
    pub exact: HashMap<NodeId, ExactState>,
    pub offset: usize,
    pub page: usize,
    pub deleting: usize,
    pub delete_errors: Vec<String>,
}

fn default_export_name() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    format!("btdua-{secs}.json")
}

impl App {
    pub fn new(live: bool) -> Self {
        App {
            dir: ROOT,
            selected: HashMap::new(),
            mode: Mode::Represented,
            sort: SortBy::Size,
            reverse: false,
            marks: BTreeSet::new(),
            dialog: Dialog::None,
            show_optional: true,
            live,
            paused: false,
            exact: HashMap::new(),
            offset: 0,
            page: 10,
            deleting: 0,
            delete_errors: Vec::new(),
        }
    }

    pub fn entries(&self, t: &Tree) -> Vec<NodeId> {
        let mut v: Vec<NodeId> = t.children(self.dir).collect();
        let name = |id: &NodeId| &t.node(*id).name;
        match self.sort {
            SortBy::Size => v.sort_by(|a, b| {
                let (va, vb) = (self.mode.value(&t.node(*a).c), self.mode.value(&t.node(*b).c));
                vb.total_cmp(&va).then_with(|| name(a).cmp(name(b)))
            }),
            SortBy::Name => v.sort_by(|a, b| name(a).cmp(name(b))),
            SortBy::Count => v.sort_by(|a, b| {
                let (ca, cb) = (t.node(*a).c.represented, t.node(*b).c.represented);
                cb.cmp(&ca).then_with(|| name(a).cmp(name(b)))
            }),
        }
        if self.reverse {
            v.reverse();
        }
        v
    }

    pub fn cursor(&self, entries: &[NodeId]) -> usize {
        match self.selected.get(&self.dir) {
            Some(&(id, hint)) => entries
                .iter()
                .position(|&e| e == id)
                .unwrap_or(hint.min(entries.len().saturating_sub(1))),
            None => 0,
        }
    }

    #[cfg(test)]
    pub fn current(&self, t: &Tree) -> Option<NodeId> {
        let e = self.entries(t);
        e.get(self.cursor(&e)).copied()
    }

    fn select(&mut self, entries: &[NodeId], idx: usize) {
        if entries.is_empty() {
            return;
        }
        let idx = idx.min(entries.len() - 1);
        self.selected.insert(self.dir, (entries[idx], idx));
    }

    fn set_sort(&mut self, s: SortBy) {
        if self.sort == s {
            self.reverse = !self.reverse;
        } else {
            self.sort = s;
            self.reverse = false;
        }
    }

    pub fn on_key(&mut self, key: KeyEvent, t: &Tree) -> Action {
        match std::mem::replace(&mut self.dialog, Dialog::None) {
            Dialog::None => self.key_browse(key, t),
            d => self.key_dialog(d, key, t),
        }
    }

    fn key_browse(&mut self, key: KeyEvent, t: &Tree) -> Action {
        let entries = self.entries(t);
        let cur = self.cursor(&entries);
        let sel = entries.get(cur).copied();
        match key.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Up | KeyCode::Char('k') => self.select(&entries, cur.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(&entries, cur + 1),
            KeyCode::PageUp => self.select(&entries, cur.saturating_sub(self.page.max(1))),
            KeyCode::PageDown => self.select(&entries, cur + self.page.max(1)),
            KeyCode::Home | KeyCode::Char('g') => self.select(&entries, 0),
            KeyCode::End | KeyCode::Char('G') => self.select(&entries, usize::MAX),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                if let Some(id) = sel.filter(|&id| !t.node(id).children.is_empty()) {
                    self.select(&entries, cur);
                    self.dir = id;
                }
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                if let Some(p) = t.node(self.dir).parent {
                    let child = self.dir;
                    self.dir = p;
                    let e = self.entries(t);
                    if let Some(i) = e.iter().position(|&x| x == child) {
                        self.select(&e, i);
                    }
                }
            }
            KeyCode::Char('s') => self.set_sort(SortBy::Size),
            KeyCode::Char('n') => self.set_sort(SortBy::Name),
            KeyCode::Char('C') => self.set_sort(SortBy::Count),
            KeyCode::Char('r') => self.reverse = !self.reverse,
            KeyCode::Char('m') => self.mode = self.mode.next(),
            KeyCode::Char('c') => self.show_optional = !self.show_optional,
            KeyCode::Char('p') if self.live => {
                self.paused = !self.paused;
                return Action::TogglePause;
            }
            KeyCode::Char(' ') => {
                if let Some(id) = sel {
                    if !t.is_special(id) && !self.marks.remove(&id) {
                        self.marks.insert(id);
                    }
                    self.select(&entries, cur + 1);
                }
            }
            KeyCode::Char('u') => self.marks.clear(),
            KeyCode::Char('i') => {
                if let Some(id) = sel {
                    self.dialog = Dialog::Info { node: id };
                }
            }
            KeyCode::Char('d') => self.begin_delete(t, sel),
            KeyCode::Char('e') => self.dialog = Dialog::Export { input: default_export_name() },
            KeyCode::Char('?') => self.dialog = Dialog::Help,
            _ => {}
        }
        Action::None
    }

    fn begin_delete(&mut self, t: &Tree, sel: Option<NodeId>) {
        if !self.live {
            self.dialog = Dialog::message("Unavailable", "Deleting needs a live filesystem (not --import).");
            return;
        }
        let mut targets: Vec<NodeId> = self.marks.iter().copied().filter(|&id| t.is_live(id)).collect();
        if targets.is_empty() {
            targets.extend(sel.filter(|&id| !t.is_special(id)));
        }
        let all = targets.clone();
        targets.retain(|&id| !all.iter().any(|&a| t.is_ancestor(a, id)));
        if !targets.is_empty() {
            self.dialog = Dialog::Confirm { targets, stage: 1 };
        }
    }

    fn key_dialog(&mut self, d: Dialog, key: KeyEvent, t: &Tree) -> Action {
        match d {
            Dialog::None | Dialog::Help | Dialog::Message { .. } => Action::None,
            Dialog::Info { node } => match key.code {
                KeyCode::Char('x') if t.is_special(node) => {
                    self.dialog = Dialog::message("Unavailable", "Special <BUCKET> entries are not filesystem paths.");
                    Action::None
                }
                KeyCode::Char('x') if self.live => {
                    self.dialog = Dialog::Info { node };
                    Action::Exact(node)
                }
                KeyCode::Char('x') => {
                    self.dialog = Dialog::message("Unavailable", "Exact size needs a live filesystem (not --import).");
                    Action::None
                }
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('i') => Action::None,
                _ => {
                    self.dialog = Dialog::Info { node };
                    Action::None
                }
            },
            Dialog::Confirm { targets, stage } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if stage == 1 && targets.iter().any(|&id| t.has_subvol(id)) {
                        self.dialog = Dialog::Confirm { targets, stage: 2 };
                        Action::None
                    } else {
                        for id in &targets {
                            self.marks.remove(id);
                        }
                        Action::Delete(targets)
                    }
                }
                _ => Action::None,
            },
            Dialog::Export { mut input } => match key.code {
                KeyCode::Enter if !input.is_empty() => Action::Export(input),
                KeyCode::Esc => Action::None,
                KeyCode::Backspace => {
                    input.pop();
                    self.dialog = Dialog::Export { input };
                    Action::None
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    self.dialog = Dialog::Export { input };
                    Action::None
                }
                _ => {
                    self.dialog = Dialog::Export { input };
                    Action::None
                }
            },
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Sample;
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn tree() -> Tree {
        let mut t = Tree::new();
        for p in ["a/x", "a/y", "a/y", "b/z"] {
            t.add_sample(&Sample { owners: vec![p.into()], ..Default::default() });
        }
        t
    }

    #[test]
    fn cursor_follows_node_across_resort() {
        let t = tree();
        let mut app = App::new(true);
        app.on_key(key(KeyCode::Down), &t);
        let b = t.find("b").unwrap();
        assert_eq!(app.current(&t), Some(b));
        app.on_key(key(KeyCode::Char('n')), &t);
        app.on_key(key(KeyCode::Char('r')), &t); // name, reversed: b first
        assert_eq!(app.current(&t), Some(b));
    }

    #[test]
    fn enter_and_leave_directory() {
        let t = tree();
        let mut app = App::new(true);
        app.on_key(key(KeyCode::Enter), &t);
        assert_eq!(app.dir, t.find("a").unwrap());
        assert_eq!(app.current(&t), t.find("a/y"));
        app.on_key(key(KeyCode::Left), &t);
        assert_eq!(app.dir, ROOT);
        assert_eq!(app.current(&t), t.find("a"));
    }

    #[test]
    fn batch_delete_dedupes_nested_targets() {
        let t = tree();
        let a = t.find("a").unwrap();
        let mut app = App::new(true);
        app.on_key(key(KeyCode::Char(' ')), &t); // mark a, cursor -> b
        app.on_key(key(KeyCode::Up), &t);
        app.on_key(key(KeyCode::Enter), &t);
        app.on_key(key(KeyCode::Char(' ')), &t); // mark a/y
        app.on_key(key(KeyCode::Left), &t);
        assert_eq!(app.marks.len(), 2);
        app.on_key(key(KeyCode::Char('d')), &t);
        assert!(matches!(&app.dialog, Dialog::Confirm { targets, stage: 1 } if targets == &vec![a]));
        assert_eq!(app.on_key(key(KeyCode::Char('y')), &t), Action::Delete(vec![a]));
        assert!(!app.marks.contains(&a));
    }

    #[test]
    fn subvolume_delete_needs_two_confirmations() {
        let mut t = Tree::new();
        t.add_sample(&Sample { owners: vec!["s/f".into()], subvols: vec!["s".into()], ratio: None });
        let s = t.find("s").unwrap();
        let mut app = App::new(true);
        app.on_key(key(KeyCode::Char('d')), &t);
        assert_eq!(app.on_key(key(KeyCode::Char('y')), &t), Action::None);
        assert!(matches!(app.dialog, Dialog::Confirm { stage: 2, .. }));
        assert_eq!(app.on_key(key(KeyCode::Char('y')), &t), Action::Delete(vec![s]));
    }

    #[test]
    fn exact_size_is_refused_for_buckets() {
        let mut t = Tree::new();
        t.add_sample(&Sample::bucket("<UNUSED>"));
        let mut app = App::new(true);
        app.on_key(key(KeyCode::Char('i')), &t);
        assert_eq!(app.on_key(key(KeyCode::Char('x')), &t), Action::None);
        assert!(matches!(app.dialog, Dialog::Message { .. }));
    }

    #[test]
    fn cursor_survives_removal() {
        let mut t = tree();
        let mut app = App::new(true);
        app.on_key(key(KeyCode::Down), &t);
        t.remove(t.find("b").unwrap());
        assert_eq!(app.current(&t), t.find("a"));
    }

    #[test]
    fn buckets_cannot_be_deleted_and_import_mode_blocks_delete() {
        let mut t = Tree::new();
        t.add_sample(&Sample::bucket("<METADATA>"));
        let mut app = App::new(true);
        app.on_key(key(KeyCode::Char('d')), &t);
        assert!(matches!(app.dialog, Dialog::None));
        let t = tree();
        let mut app = App::new(false);
        app.on_key(key(KeyCode::Char('d')), &t);
        assert!(matches!(app.dialog, Dialog::Message { .. }));
    }
}
