//! Arena-allocated path trie holding per-node sample counters.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub type NodeId = u32;
pub const ROOT: NodeId = 0;
pub const MAX_SHARERS: usize = 5;

/// One resolved sample.
#[derive(Debug, Clone, Default)]
pub struct Sample {
    /// Owner paths relative to the fs top level ("a/b/c"), canonical owner first.
    /// Non-file samples use a single bucket path such as "<METADATA>".
    pub owners: Vec<String>,
    /// Subvolume root paths seen while resolving (flags those nodes).
    pub subvols: Vec<String>,
    /// ram_bytes / disk_bytes of the canonical owner's extent, when known.
    pub ratio: Option<f64>,
}

impl Sample {
    pub fn bucket(path: &str) -> Self {
        Sample { owners: vec![path.to_string()], ..Default::default() }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Counters {
    pub represented: u64,
    pub distributed: f64,
    pub exclusive: u64,
    pub shared: u64,
    pub comp_samples: u64,
    pub uncompressed: f64,
}

impl Counters {
    /// Disk bytes per uncompressed byte; `None` when no sample had a known ratio.
    pub fn ratio(&self) -> Option<f64> {
        (self.comp_samples > 0 && self.uncompressed > 0.0).then(|| self.comp_samples as f64 / self.uncompressed)
    }

    fn subtract(&mut self, o: &Counters) {
        self.represented = self.represented.saturating_sub(o.represented);
        self.distributed = (self.distributed - o.distributed).max(0.0);
        self.exclusive = self.exclusive.saturating_sub(o.exclusive);
        self.shared = self.shared.saturating_sub(o.shared);
        self.comp_samples = self.comp_samples.saturating_sub(o.comp_samples);
        self.uncompressed = (self.uncompressed - o.uncompressed).max(0.0);
    }
}

#[derive(Debug)]
pub struct Node {
    pub name: Box<str>,
    pub parent: Option<NodeId>,
    pub children: HashMap<Box<str>, NodeId>,
    pub subvol: bool,
    pub removed: bool,
    pub c: Counters,
    /// Other owner paths recently seen sharing an extent with this node.
    pub sharers: Vec<Box<str>>,
}

impl Node {
    fn new(name: &str, parent: Option<NodeId>) -> Self {
        Node {
            name: name.into(),
            parent,
            children: HashMap::new(),
            subvol: false,
            removed: false,
            c: Counters::default(),
            sharers: Vec::new(),
        }
    }
}

pub struct Tree {
    nodes: Vec<Node>,
    pub total_samples: u64,
    hits: HashMap<NodeId, u32>,
}

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

impl Tree {
    pub fn new() -> Self {
        Tree { nodes: vec![Node::new("", None)], total_samples: 0, hits: HashMap::new() }
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    #[cfg(test)]
    pub fn find(&self, path: &str) -> Option<NodeId> {
        let mut cur = ROOT;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            cur = *self.nodes[cur as usize].children.get(comp)?;
        }
        Some(cur)
    }

    pub fn add_child(&mut self, parent: NodeId, name: &str) -> NodeId {
        if let Some(&id) = self.nodes[parent as usize].children.get(name) {
            return id;
        }
        let id = self.nodes.len() as NodeId;
        self.nodes.push(Node::new(name, Some(parent)));
        self.nodes[parent as usize].children.insert(name.into(), id);
        id
    }

    pub fn get_or_create(&mut self, path: &str) -> NodeId {
        let mut cur = ROOT;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            cur = self.add_child(cur, comp);
        }
        cur
    }

    pub fn set_counters(&mut self, id: NodeId, c: Counters, subvol: bool) {
        let n = &mut self.nodes[id as usize];
        n.c = c;
        n.subvol = subvol;
    }

    pub fn add_sample(&mut self, s: &Sample) {
        if s.owners.is_empty() {
            return;
        }
        self.total_samples += 1;
        for sv in &s.subvols {
            let id = self.get_or_create(sv);
            self.nodes[id as usize].subvol = true;
        }
        let leaves: Vec<NodeId> = s.owners.iter().map(|p| self.get_or_create(p)).collect();
        let m = leaves.len() as u32;
        let frac = 1.0 / m as f64;
        let mut hits = std::mem::take(&mut self.hits);
        hits.clear();
        for (i, &leaf) in leaves.iter().enumerate() {
            let mut cur = Some(leaf);
            while let Some(id) = cur {
                let n = &mut self.nodes[id as usize];
                n.c.distributed += frac;
                if i == 0 {
                    n.c.represented += 1;
                    if let Some(r) = s.ratio {
                        n.c.comp_samples += 1;
                        n.c.uncompressed += r;
                    }
                }
                *hits.entry(id).or_insert(0) += 1;
                cur = n.parent;
            }
        }
        for (&id, &cnt) in &hits {
            let c = &mut self.nodes[id as usize].c;
            if cnt == m { c.exclusive += 1 } else { c.shared += 1 }
        }
        self.hits = hits;
        if m > 1 {
            for (i, &leaf) in leaves.iter().enumerate() {
                let others = s.owners.iter().enumerate().filter(|&(j, _)| j != i).take(MAX_SHARERS);
                for (_, p) in others {
                    let sh = &mut self.nodes[leaf as usize].sharers;
                    if sh.iter().any(|x| **x == **p) {
                        continue;
                    }
                    if sh.len() == MAX_SHARERS {
                        sh.remove(0);
                    }
                    sh.push(p.as_str().into());
                }
            }
        }
    }

    pub fn children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.nodes[id as usize].children.values().copied()
    }

    pub fn path_of(&self, id: NodeId) -> String {
        let mut parts = Vec::new();
        let mut cur = id;
        while let Some(p) = self.nodes[cur as usize].parent {
            parts.push(&*self.nodes[cur as usize].name);
            cur = p;
        }
        parts.reverse();
        parts.join("/")
    }

    /// True for `<BUCKET>` nodes and everything below them.
    pub fn is_special(&self, id: NodeId) -> bool {
        let mut cur = id;
        while let Some(p) = self.nodes[cur as usize].parent {
            if p == ROOT {
                return self.nodes[cur as usize].name.starts_with('<');
            }
            cur = p;
        }
        false
    }

    /// Strict ancestor test.
    pub fn is_ancestor(&self, anc: NodeId, id: NodeId) -> bool {
        let mut cur = self.nodes[id as usize].parent;
        while let Some(p) = cur {
            if p == anc {
                return true;
            }
            cur = self.nodes[p as usize].parent;
        }
        false
    }

    pub fn has_subvol(&self, id: NodeId) -> bool {
        let n = &self.nodes[id as usize];
        n.subvol || n.children.values().any(|&c| self.has_subvol(c))
    }

    pub fn is_live(&self, id: NodeId) -> bool {
        let mut cur = Some(id);
        while let Some(c) = cur {
            if self.nodes[c as usize].removed {
                return false;
            }
            cur = self.nodes[c as usize].parent;
        }
        true
    }

    /// Detaches `id` and subtracts its counters from every ancestor.
    pub fn remove(&mut self, id: NodeId) {
        if id == ROOT || !self.is_live(id) {
            return;
        }
        let c = self.nodes[id as usize].c;
        let mut cur = self.nodes[id as usize].parent;
        while let Some(p) = cur {
            self.nodes[p as usize].c.subtract(&c);
            cur = self.nodes[p as usize].parent;
        }
        let parent = self.nodes[id as usize].parent.unwrap();
        let name = self.nodes[id as usize].name.clone();
        self.nodes[parent as usize].children.remove(&name);
        self.nodes[id as usize].removed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(owners: &[&str]) -> Sample {
        Sample { owners: owners.iter().map(|o| o.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn single_owner_counts_up_the_chain() {
        let mut t = Tree::new();
        t.add_sample(&s(&["a/b/c"]));
        for p in ["", "a", "a/b", "a/b/c"] {
            let c = t.node(t.find(p).unwrap()).c;
            assert_eq!((c.represented, c.exclusive, c.shared), (1, 1, 0), "{p}");
            assert_eq!(c.distributed, 1.0);
        }
        assert_eq!(t.total_samples, 1);
    }

    #[test]
    fn shared_between_subtrees() {
        let mut t = Tree::new();
        t.add_sample(&s(&["a/x", "b/y"]));
        let (a, b, x) = (t.find("a").unwrap(), t.find("b").unwrap(), t.find("a/x").unwrap());
        assert_eq!(t.node(a).c.represented, 1);
        assert_eq!(t.node(b).c.represented, 0);
        assert_eq!(t.node(b).c.distributed, 0.5);
        assert_eq!((t.node(a).c.exclusive, t.node(a).c.shared), (0, 1));
        assert_eq!((t.node(ROOT).c.exclusive, t.node(ROOT).c.shared), (1, 0));
        assert_eq!(&*t.node(x).sharers[0], "b/y");
    }

    #[test]
    fn shared_within_one_dir_is_exclusive_to_it() {
        let mut t = Tree::new();
        t.add_sample(&s(&["d/x", "d/y"]));
        let d = t.find("d").unwrap();
        assert_eq!((t.node(d).c.exclusive, t.node(d).c.shared), (1, 0));
        assert_eq!(t.node(d).c.represented, 1);
    }

    #[test]
    fn remove_subtracts_and_detaches() {
        let mut t = Tree::new();
        t.add_sample(&s(&["a/x"]));
        t.add_sample(&s(&["a/y"]));
        let x = t.find("a/x").unwrap();
        t.remove(x);
        assert!(t.find("a/x").is_none());
        assert!(!t.is_live(x));
        assert_eq!(t.node(t.find("a").unwrap()).c.represented, 1);
        assert_eq!(t.total_samples, 2);
    }

    #[test]
    fn flags_subvols_and_specials() {
        let mut t = Tree::new();
        t.add_sample(&Sample { owners: vec!["s/f".into()], subvols: vec!["s".into()], ratio: Some(4.0) });
        t.add_sample(&Sample::bucket("<ERROR>/EIO"));
        let sv = t.find("s").unwrap();
        assert!(t.node(sv).subvol && t.has_subvol(ROOT));
        assert!(t.is_special(t.find("<ERROR>/EIO").unwrap()));
        assert!(!t.is_special(sv));
        assert_eq!(t.node(sv).c.ratio(), Some(0.25));
    }
}
