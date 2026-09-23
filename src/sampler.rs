//! Random sampling of logical space and resolution into owner paths.

use std::collections::HashMap;
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};

use crate::btrfs::{self, Chunk, errno_name};
use crate::fsat;
use crate::fsopen::FsHandle;
use crate::rng::Rng;
use crate::tree::{Sample, Tree};

pub struct ChunkMap {
    chunks: Vec<Chunk>,
    ends: Vec<u64>,
    pub total: u64,
}

impl ChunkMap {
    pub fn new(mut chunks: Vec<Chunk>) -> Self {
        chunks.sort_by_key(|c| c.start);
        let mut total = 0;
        let ends = chunks.iter().map(|c| { total += c.len; total }).collect();
        ChunkMap { chunks, ends, total }
    }

    /// Maps `r` in `[0, total)` to (logical address, chunk flags).
    pub fn pick(&self, r: u64) -> (u64, u64) {
        let i = self.ends.partition_point(|&e| e <= r);
        let before = if i == 0 { 0 } else { self.ends[i - 1] };
        let c = &self.chunks[i];
        (c.start + (r - before), c.flags)
    }
}

/// Subvolume cache whose inserts are dropped if it was invalidated after
/// the caller started its (lock-free) lookup.
struct Cache<V> {
    generation: u64,
    map: HashMap<u64, V>,
}

impl<V: Clone> Cache<V> {
    fn new() -> Self {
        Cache { generation: 0, map: HashMap::new() }
    }

    fn get(&self, k: u64) -> Option<V> {
        self.map.get(&k).cloned()
    }

    fn insert(&mut self, generation: u64, k: u64, v: V) {
        if generation == self.generation {
            self.map.insert(k, v);
        }
    }

    fn invalidate(&mut self) {
        self.generation += 1;
        self.map.clear();
    }
}

#[derive(Clone)]
struct Subvol {
    path: String,
    fd: Arc<File>,
}

pub struct Resolver {
    fs: Arc<FsHandle>,
    subvols: Mutex<Cache<Option<Subvol>>>,
}

fn join_path(a: &str, b: &str) -> String {
    let b = b.trim_matches('/');
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a}/{b}"),
    }
}

impl Resolver {
    pub fn new(fs: Arc<FsHandle>) -> Self {
        Resolver { fs, subvols: Mutex::new(Cache::new()) }
    }

    /// Forgets cached subvolume paths and fds (after something was deleted).
    pub fn invalidate(&self) {
        self.subvols.lock().invalidate();
    }

    fn subvol(&self, root: u64) -> Option<Subvol> {
        let generation = {
            let c = self.subvols.lock();
            if let Some(s) = c.get(root) {
                return s;
            }
            c.generation
        };
        let s = self.open_subvol(root);
        self.subvols.lock().insert(generation, root, s.clone());
        s
    }

    fn subvol_path(&self, root: u64, depth: u32) -> Option<String> {
        if root == btrfs::FS_TREE_OBJECTID {
            return Some(String::new());
        }
        if depth > 64 {
            return None;
        }
        let (parent, dirid, name) = btrfs::root_backref(&self.fs.top, root).ok()??;
        let parent_path = self.subvol_path(parent, depth + 1)?;
        let (_, dir) = btrfs::ino_lookup(&self.fs.top, parent, dirid).ok()?;
        Some(join_path(&parent_path, &format!("{dir}{name}")))
    }

    fn open_subvol(&self, root: u64) -> Option<Subvol> {
        let path = self.subvol_path(root, 0)?;
        let fd = fsat::open_dir_beneath(&self.fs.top, &path).ok()?;
        let (actual, _) = btrfs::ino_lookup(&fd, 0, btrfs::FIRST_FREE_OBJECTID).ok()?;
        (actual == root).then(|| Subvol { path, fd: Arc::new(fd) })
    }

    /// ram_bytes / disk_bytes of the extent holding `logical` (1.0 when uncompressed).
    fn ratio(&self, root: u64, inum: u64, file_offset: u64, logical: u64) -> Option<f64> {
        let mut found = None;
        let lo = file_offset.saturating_sub(128 << 10);
        btrfs::file_extents(&self.fs.top, root, inum, lo, file_offset, |fe| {
            if fe.disk_bytenr != 0 && fe.disk_bytenr <= logical && logical < fe.disk_bytenr + fe.disk_num_bytes {
                found = Some(*fe);
            }
            true
        })
        .ok()?;
        Some(match found {
            Some(fe) if fe.compression != 0 && fe.disk_num_bytes > 0 => fe.ram_bytes as f64 / fe.disk_num_bytes as f64,
            _ => 1.0,
        })
    }

    pub fn resolve(&self, logical: u64, flags: u64, buf: &mut Vec<u8>) -> Sample {
        if flags & btrfs::BLOCK_GROUP_DATA == 0 {
            let b = if flags & btrfs::BLOCK_GROUP_SYSTEM != 0 { "<SYSTEM>" } else { "<METADATA>" };
            return Sample::bucket(b);
        }
        let refs = match btrfs::logical_ino(&self.fs.top, logical, buf) {
            Ok(r) => r,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Sample::bucket("<UNUSED>"),
            Err(e) => return Sample::bucket(&format!("<ERROR>/LOGICAL_INO {}", errno_name(&e))),
        };
        if refs.is_empty() {
            return Sample::bucket("<UNREACHABLE>");
        }
        // (sort key root, path, index into refs)
        let mut owners: Vec<(u64, String, Option<usize>)> = Vec::new();
        let mut subvols = Vec::new();
        for (i, r) in refs.iter().enumerate() {
            let Some(sv) = self.subvol(r.root) else {
                owners.push((u64::MAX, format!("<UNREACHABLE>/subvol {}", r.root), None));
                continue;
            };
            match btrfs::ino_paths(&sv.fd, r.inum) {
                Ok(paths) if !paths.is_empty() => {
                    for p in paths {
                        owners.push((r.root, join_path(&sv.path, &p), Some(i)));
                    }
                    if !sv.path.is_empty() {
                        subvols.push(sv.path.clone());
                    }
                }
                Ok(_) => owners.push((u64::MAX, "<UNREACHABLE>/unlinked".into(), None)),
                Err(e) => owners.push((u64::MAX, format!("<ERROR>/INO_PATHS {}", errno_name(&e)), None)),
            }
        }
        owners.sort_by(|a, b| (a.0, a.1.len(), &a.1).cmp(&(b.0, b.1.len(), &b.1)));
        owners.dedup_by(|a, b| a.1 == b.1);
        subvols.sort();
        subvols.dedup();
        let ratio = owners[0].2.and_then(|i| {
            let r = &refs[i];
            self.ratio(r.root, r.inum, r.offset, logical)
        });
        Sample { owners: owners.into_iter().map(|o| o.1).collect(), subvols, ratio }
    }
}

pub struct Sampler {
    stop: Arc<AtomicBool>,
    pub paused: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
}

impl Sampler {
    pub fn start(resolver: Arc<Resolver>, map: Arc<ChunkMap>, threads: usize, seed: u64, tree: Arc<RwLock<Tree>>) -> Self {
        let (tx, rx) = crossbeam_channel::bounded::<Sample>(4096);
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let workers = (0..threads.max(1) as u64)
            .map(|i| {
                let (resolver, map, tx) = (resolver.clone(), map.clone(), tx.clone());
                let (stop, paused) = (stop.clone(), paused.clone());
                thread::spawn(move || {
                    let mut rng = Rng::new(seed ^ i.wrapping_mul(0xA24B_AED4_963E_E407));
                    let mut buf = vec![0u8; 64 << 10];
                    while !stop.load(Relaxed) {
                        if paused.load(Relaxed) {
                            thread::sleep(Duration::from_millis(100));
                            continue;
                        }
                        let (logical, flags) = map.pick(rng.below(map.total));
                        if tx.send(resolver.resolve(logical, flags, &mut buf)).is_err() {
                            break;
                        }
                    }
                })
            })
            .collect();
        thread::spawn(move || {
            while let Ok(s) = rx.recv() {
                let mut t = tree.write();
                t.add_sample(&s);
                for s in rx.try_iter().take(4096) {
                    t.add_sample(&s);
                }
            }
        });
        Sampler { stop, paused, workers }
    }

    pub fn stop(self) {
        self.stop.store(true, Relaxed);
        for w in self.workers {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_map_picks_by_weight() {
        let m = ChunkMap::new(vec![
            Chunk { start: 1000, len: 10, flags: 4 },
            Chunk { start: 0, len: 5, flags: 1 },
        ]);
        assert_eq!(m.total, 15);
        assert_eq!(m.pick(0), (0, 1));
        assert_eq!(m.pick(4), (4, 1));
        assert_eq!(m.pick(5), (1000, 4));
        assert_eq!(m.pick(14), (1009, 4));
    }

    #[test]
    fn cache_drops_inserts_that_started_before_invalidate() {
        let mut c = Cache::new();
        let started = c.generation;
        c.invalidate();
        c.insert(started, 1, "stale");
        assert_eq!(c.get(1), None);
        c.insert(c.generation, 2, "fresh");
        assert_eq!(c.get(2), Some("fresh"));
    }

    #[test]
    fn joins_paths() {
        assert_eq!(join_path("", "a/b"), "a/b");
        assert_eq!(join_path("@home", "u/f"), "@home/u/f");
        assert_eq!(join_path("x", "y/"), "x/y");
    }
}
