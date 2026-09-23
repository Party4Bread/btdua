//! Path resolution relative to the top-level directory fd.
//!
//! Paths are walked one component at a time with `O_NOFOLLOW`, and `.`/`..`
//! are rejected, so neither a planted nor a concurrently swapped symlink can
//! redirect an operation (which runs as root) outside the filesystem tree.

use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;

use crate::btrfs;

fn cstr(name: &OsStr) -> io::Result<CString> {
    Ok(CString::new(name.as_bytes())?)
}

fn check(r: libc::c_int) -> io::Result<()> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

pub fn open_dir_at(dir: &File, name: &OsStr) -> io::Result<File> {
    let c = cstr(name)?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: valid dir fd and NUL-terminated name; a returned fd is owned by the File.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags) };
    check(fd)?;
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub fn stat_at(dir: &File, name: &OsStr) -> io::Result<libc::stat> {
    let c = cstr(name)?;
    // SAFETY: stat is plain data; fstatat fills it.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    check(unsafe { libc::fstatat(dir.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
    Ok(st)
}

fn fstat(f: &File) -> io::Result<libc::stat> {
    // SAFETY: as above.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    check(unsafe { libc::fstat(f.as_raw_fd(), &mut st) })?;
    Ok(st)
}

fn unlink_at(dir: &File, name: &OsStr, flags: libc::c_int) -> io::Result<()> {
    let c = cstr(name)?;
    // SAFETY: valid dir fd and NUL-terminated name.
    check(unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), flags) })
}

pub fn is_dir(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}

pub fn is_file(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFREG
}

pub fn is_subvol_root(st: &libc::stat) -> bool {
    is_dir(st) && st.st_ino == btrfs::FIRST_FREE_OBJECTID
}

/// Names in `dir`, listed through the fd itself (not by path).
pub fn entries(dir: &File) -> io::Result<Vec<OsString>> {
    fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))?
        .map(|e| e.map(|e| e.file_name()))
        .collect()
}

fn components(rel: &str) -> io::Result<Vec<&OsStr>> {
    let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
    if comps.iter().any(|c| *c == "." || *c == "..") {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "path contains . or .."));
    }
    Ok(comps.into_iter().map(OsStr::new).collect())
}

/// Opens directory `rel` below `top` ("" is `top` itself).
pub fn open_dir_beneath(top: &File, rel: &str) -> io::Result<File> {
    let mut dir = top.try_clone()?;
    for c in components(rel)? {
        dir = open_dir_at(&dir, c)?;
    }
    Ok(dir)
}

/// Opens the parent directory of `rel` and returns it with the final name.
pub fn open_parent(top: &File, rel: &str) -> io::Result<(File, OsString)> {
    let comps = components(rel)?;
    let (last, dirs) = comps.split_last().ok_or(io::ErrorKind::InvalidInput)?;
    let mut dir = top.try_clone()?;
    for c in dirs {
        dir = open_dir_at(&dir, c)?;
    }
    Ok((dir, last.to_os_string()))
}

/// Deletes `rel` below `top`: a file, directory tree or subvolume (nested
/// subvolumes first).
pub fn delete_rel(top: &File, rel: &str) -> io::Result<()> {
    let (parent, name) = open_parent(top, rel)?;
    delete_at(&parent, &name)
}

fn delete_at(parent: &File, name: &OsStr) -> io::Result<()> {
    let st = stat_at(parent, name)?;
    if !is_dir(&st) {
        return unlink_at(parent, name, 0);
    }
    let dir = open_dir_at(parent, name)?;
    let opened = fstat(&dir)?;
    if (opened.st_dev, opened.st_ino) != (st.st_dev, st.st_ino) {
        return Err(io::Error::other("directory changed while deleting"));
    }
    if is_subvol_root(&st) {
        destroy_nested(&dir)?;
        drop(dir);
        btrfs::snap_destroy(parent, name)
    } else {
        for e in entries(&dir)? {
            delete_at(&dir, &e)?;
        }
        unlink_at(parent, name, libc::AT_REMOVEDIR)
    }
}

/// Destroys every subvolume nested anywhere below `dir`, leaving other files.
fn destroy_nested(dir: &File) -> io::Result<()> {
    for e in entries(dir)? {
        let st = stat_at(dir, &e)?;
        if is_subvol_root(&st) {
            delete_at(dir, &e)?;
        } else if is_dir(&st) {
            destroy_nested(&open_dir_at(dir, &e)?)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("btdua-fsat-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("top")).unwrap();
        fs::create_dir_all(d.join("outside")).unwrap();
        fs::write(d.join("outside/x"), b"keep").unwrap();
        d
    }

    fn top(d: &Path) -> File {
        File::open(d.join("top")).unwrap()
    }

    #[test]
    fn delete_does_not_follow_symlinked_ancestors() {
        let d = scratch("anc");
        symlink(d.join("outside"), d.join("top/link")).unwrap();
        assert!(delete_rel(&top(&d), "link/x").is_err());
        assert!(d.join("outside/x").exists());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn delete_removes_symlink_not_its_target() {
        let d = scratch("leaf");
        fs::create_dir_all(d.join("top/dir")).unwrap();
        symlink(d.join("outside"), d.join("top/dir/link")).unwrap();
        delete_rel(&top(&d), "dir").unwrap();
        assert!(!d.join("top/dir").exists());
        assert!(d.join("outside/x").exists());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn delete_rejects_dot_dot() {
        let d = scratch("dotdot");
        assert!(delete_rel(&top(&d), "../outside/x").is_err());
        assert!(d.join("outside/x").exists());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn deletes_plain_directory_tree() {
        let d = scratch("plain");
        fs::create_dir_all(d.join("top/a/b")).unwrap();
        fs::write(d.join("top/a/b/f"), b"x").unwrap();
        delete_rel(&top(&d), "a").unwrap();
        assert!(!d.join("top/a").exists());
        fs::remove_dir_all(&d).unwrap();
    }
}
