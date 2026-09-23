//! JSON export/import of a sampling session.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::tree::{Counters, NodeId, ROOT, Tree};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Meta {
    pub version: u32,
    pub fsid: String,
    pub device: String,
    /// Sum of allocated chunk lengths (the sampled space).
    pub total_bytes: u64,
    pub disk_bytes: u64,
    pub samples: u64,
    pub timestamp: u64,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Serialize, Deserialize)]
struct JNode {
    name: String,
    #[serde(default, skip_serializing_if = "is_false")]
    subvol: bool,
    #[serde(flatten)]
    c: Counters,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    children: Vec<JNode>,
}

#[derive(Serialize, Deserialize)]
struct Doc {
    meta: Meta,
    root: JNode,
}

fn to_json(t: &Tree, id: NodeId) -> JNode {
    let n = t.node(id);
    let mut children: Vec<JNode> = t.children(id).map(|c| to_json(t, c)).collect();
    children.sort_by(|a, b| a.name.cmp(&b.name));
    JNode { name: n.name.to_string(), subvol: n.subvol, c: n.c, children }
}

fn from_json(t: &mut Tree, id: NodeId, j: &JNode) {
    t.set_counters(id, j.c, j.subvol);
    for c in &j.children {
        let cid = t.add_child(id, &c.name);
        from_json(t, cid, c);
    }
}

pub fn write(path: &Path, meta: &Meta, tree: &Tree) -> Result<()> {
    let doc = Doc { meta: meta.clone(), root: to_json(tree, ROOT) };
    let f = File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
    serde_json::to_writer(BufWriter::new(f), &doc)?;
    Ok(())
}

pub fn read(path: &Path) -> Result<(Meta, Tree)> {
    let f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut de = serde_json::Deserializer::from_reader(BufReader::new(f));
    de.disable_recursion_limit();
    let doc = Doc::deserialize(&mut de).context("invalid btdua export")?;
    let mut t = Tree::new();
    t.total_samples = doc.meta.samples;
    from_json(&mut t, ROOT, &doc.root);
    Ok((doc.meta, t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Sample;

    #[test]
    fn round_trips() {
        let mut t = Tree::new();
        t.add_sample(&Sample { owners: vec!["s/a".into(), "b".into()], subvols: vec!["s".into()], ratio: Some(2.0) });
        t.add_sample(&Sample::bucket("<METADATA>"));
        let meta = Meta { version: 1, samples: t.total_samples, total_bytes: 100, ..Default::default() };
        let p = std::env::temp_dir().join(format!("btdua-test-{}.json", std::process::id()));
        write(&p, &meta, &t).unwrap();
        let (m2, t2) = read(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        assert_eq!(m2.samples, 2);
        assert_eq!(t2.total_samples, 2);
        let a = t2.find("s/a").unwrap();
        assert_eq!(t2.node(a).c, t.node(t.find("s/a").unwrap()).c);
        assert!(t2.node(t2.find("s").unwrap()).subvol);
    }
}
