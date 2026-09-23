//! Expensive read-only operations run off the UI thread.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::File;
use std::io;

use crate::btrfs;
use crate::fsat;

/// Exact, compsize-style usage of a directory tree.
#[derive(Debug, Default, Clone)]
pub struct Exact {
    /// Unique on-disk bytes of all extents referenced.
    pub disk: u64,
    /// Uncompressed size of those unique extents.
    pub uncompressed: u64,
    /// Bytes of file data referenced (counting shared extents each time).
    pub referenced: u64,
    pub files: u64,
}

/// Walks `rel` below `top`, summing unique extents of every regular file
/// (compsize-style). Never opens files, only directories.
pub fn exact_size(top: &File, rel: &str) -> io::Result<Exact> {
    let (parent, name) = fsat::open_parent(top, rel)?;
    let (root, _) = btrfs::ino_lookup(&parent, 0, btrfs::FIRST_FREE_OBJECTID)?;
    let mut ex = Exact::default();
    let mut seen = HashSet::new();
    walk_at(top, &parent, &name, root, &mut seen, &mut ex)?;
    Ok(ex)
}

fn walk_at(top: &File, parent: &File, name: &OsStr, root: u64, seen: &mut HashSet<u64>, ex: &mut Exact) -> io::Result<()> {
    let st = fsat::stat_at(parent, name)?;
    if fsat::is_dir(&st) {
        let dir = fsat::open_dir_at(parent, name)?;
        let root = if fsat::is_subvol_root(&st) {
            btrfs::ino_lookup(&dir, 0, btrfs::FIRST_FREE_OBJECTID)?.0
        } else {
            root
        };
        for e in fsat::entries(&dir)? {
            let _ = walk_at(top, &dir, &e, root, seen, ex);
        }
    } else if fsat::is_file(&st) {
        ex.files += 1;
        btrfs::file_extents(top, root, st.st_ino, 0, u64::MAX, |fe| {
            if fe.kind == 0 {
                ex.referenced += fe.ram_bytes;
                ex.disk += fe.ram_bytes;
                ex.uncompressed += fe.ram_bytes;
            } else if fe.disk_bytenr != 0 {
                ex.referenced += fe.num_bytes;
                if seen.insert(fe.disk_bytenr) {
                    ex.disk += fe.disk_num_bytes;
                    ex.uncompressed += fe.ram_bytes;
                }
            }
            true
        })?;
    }
    Ok(())
}
