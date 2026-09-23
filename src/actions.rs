//! Filesystem-changing and expensive operations run off the UI thread.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::btrfs;

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

fn is_subvol_root(md: &fs::Metadata) -> bool {
    md.is_dir() && md.ino() == btrfs::FIRST_FREE_OBJECTID
}

/// Deletes a file, directory tree or subvolume (nested subvolumes first).
pub fn delete_path(p: &Path) -> io::Result<()> {
    let md = fs::symlink_metadata(p)?;
    if is_subvol_root(&md) {
        destroy_nested(p)?;
        let parent = File::open(p.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
        btrfs::snap_destroy(&parent, p.file_name().ok_or(io::ErrorKind::InvalidInput)?)
    } else if md.is_dir() {
        for e in fs::read_dir(p)? {
            delete_path(&e?.path())?;
        }
        fs::remove_dir(p)
    } else {
        fs::remove_file(p)
    }
}

/// Destroys every subvolume nested anywhere below `p`, leaving other files.
fn destroy_nested(p: &Path) -> io::Result<()> {
    for e in fs::read_dir(p)? {
        let e = e?;
        if !e.file_type()?.is_dir() {
            continue;
        }
        let child = e.path();
        if is_subvol_root(&fs::symlink_metadata(&child)?) {
            delete_path(&child)?;
        } else {
            destroy_nested(&child)?;
        }
    }
    Ok(())
}

/// Walks `path`, summing unique extents of every regular file (compsize-style).
pub fn exact_size(top: &File, path: &Path) -> io::Result<Exact> {
    let (root, _) = btrfs::ino_lookup(&File::open(path)?, 0, btrfs::FIRST_FREE_OBJECTID)?;
    let mut ex = Exact::default();
    let mut seen = HashSet::new();
    walk(top, path, root, &mut seen, &mut ex)?;
    Ok(ex)
}

fn walk(top: &File, p: &Path, root: u64, seen: &mut HashSet<u64>, ex: &mut Exact) -> io::Result<()> {
    let md = fs::symlink_metadata(p)?;
    if md.is_dir() {
        let root = if is_subvol_root(&md) {
            btrfs::ino_lookup(&File::open(p)?, 0, btrfs::FIRST_FREE_OBJECTID)?.0
        } else {
            root
        };
        for e in fs::read_dir(p)?.flatten() {
            let _ = walk(top, &e.path(), root, seen, ex);
        }
    } else if md.is_file() {
        ex.files += 1;
        btrfs::file_extents(top, root, md.ino(), 0, u64::MAX, |fe| {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletes_plain_directory_tree() {
        let d = std::env::temp_dir().join(format!("btdua-del-{}", std::process::id()));
        fs::create_dir_all(d.join("a/b")).unwrap();
        fs::write(d.join("a/b/f"), b"x").unwrap();
        fs::write(d.join("g"), b"y").unwrap();
        delete_path(&d).unwrap();
        assert!(!d.exists());
    }
}