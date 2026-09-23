//! Opening the filesystem at its top-level subvolume (subvolid 5).

use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::btrfs;

const BTRFS_SUPER_MAGIC: i64 = 0x9123_683E;

pub struct FsHandle {
    /// Directory where the top-level subvolume is reachable.
    pub mount: PathBuf,
    pub top: File,
    pub device: String,
    temp_mount: bool,
}

impl FsHandle {
    /// Must run before any threads are spawned: it may unshare the mount namespace.
    pub fn open(path: &Path) -> Result<Self> {
        let path = path.canonicalize().with_context(|| format!("cannot access {}", path.display()))?;
        let f = File::open(&path).with_context(|| format!("cannot open {}", path.display()))?;
        if fs_type(&f)? != BTRFS_SUPER_MAGIC {
            bail!("{} is not on a btrfs filesystem", path.display());
        }
        let (tree, _) = btrfs::ino_lookup(&f, 0, btrfs::FIRST_FREE_OBJECTID).map_err(root_hint)?;
        let device = mount_source(&path)?;
        if tree == btrfs::FS_TREE_OBJECTID && f.metadata()?.ino() == btrfs::FIRST_FREE_OBJECTID {
            return Ok(FsHandle { mount: path, top: f, device, temp_mount: false });
        }
        let dir = std::env::temp_dir().join(format!("btdua-{}", std::process::id()));
        private_mount(&device, &dir)
            .map_err(root_hint)
            .with_context(|| format!("mounting the top-level subvolume of {device}"))?;
        let top = File::open(&dir)?;
        Ok(FsHandle { mount: dir, top, device, temp_mount: true })
    }
}

impl Drop for FsHandle {
    fn drop(&mut self) {
        if !self.temp_mount {
            return;
        }
        if let Ok(p) = CString::new(self.mount.as_os_str().as_bytes()) {
            // SAFETY: valid NUL-terminated path.
            unsafe { libc::umount2(p.as_ptr(), libc::MNT_DETACH) };
        }
        let _ = fs::remove_dir(&self.mount);
    }
}

fn root_hint(e: io::Error) -> anyhow::Error {
    if matches!(e.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES)) {
        anyhow!("permission denied: btdua needs root (try sudo), or use --import FILE to view saved results")
    } else {
        e.into()
    }
}

fn fs_type(f: &File) -> io::Result<i64> {
    // SAFETY: statfs is plain data; fstatfs fills it.
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(f.as_raw_fd(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(s.f_type as i64)
}

pub fn disk_bytes(path: &Path) -> io::Result<u64> {
    let p = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: statvfs is plain data; statvfs() fills it.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(p.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(s.f_blocks as u64 * s.f_frsize as u64)
}

fn private_mount(device: &str, dir: &Path) -> io::Result<()> {
    fn check(r: libc::c_int) -> io::Result<()> {
        if r != 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }
    // SAFETY: constant C strings; the process is still single-threaded.
    unsafe {
        check(libc::unshare(libc::CLONE_NEWNS))?;
        check(libc::mount(
            c"none".as_ptr(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        ))?;
    }
    fs::create_dir_all(dir)?;
    let dev = CString::new(device)?;
    let target = CString::new(dir.as_os_str().as_bytes())?;
    // SAFETY: valid NUL-terminated strings.
    check(unsafe {
        libc::mount(dev.as_ptr(), target.as_ptr(), c"btrfs".as_ptr(), 0, c"subvolid=5".as_ptr().cast())
    })
}

fn mount_source(path: &Path) -> Result<String> {
    let info = fs::read_to_string("/proc/self/mountinfo")?;
    let mut best: Option<(usize, String)> = None;
    for line in info.lines() {
        let Some((pre, post)) = line.split_once(" - ") else { continue };
        let mut post = post.split(' ');
        let (Some(fstype), Some(source)) = (post.next(), post.next()) else { continue };
        let Some(mp) = pre.split(' ').nth(4) else { continue };
        if fstype != "btrfs" {
            continue;
        }
        let mp = PathBuf::from(unescape(mp));
        let len = mp.as_os_str().len();
        if path.starts_with(&mp) && best.as_ref().is_none_or(|b| len > b.0) {
            best = Some((len, unescape(source)));
        }
    }
    best.map(|b| b.1).context("could not find this path's btrfs mount in /proc/self/mountinfo")
}

/// Decodes mountinfo's `\ooo` octal escapes.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push(u8::from_str_radix(&s[i + 1..i + 4], 8).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn unescapes_mountinfo() {
        assert_eq!(super::unescape(r"/mnt/my\040disk"), "/mnt/my disk");
        assert_eq!(super::unescape("/plain"), "/plain");
    }
}
