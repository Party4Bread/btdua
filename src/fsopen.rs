//! Opening the filesystem at its top-level subvolume (subvolid 5).

use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::btrfs;
use crate::rng::{self, Rng};

const BTRFS_SUPER_MAGIC: i64 = 0x9123_683E;

pub struct FsHandle {
    /// Directory where the top-level subvolume is reachable.
    pub mount: PathBuf,
    pub top: File,
    pub device: String,
    /// Paths of this device that are mounted on the system (never deleted).
    pub mounted: Vec<String>,
}

impl FsHandle {
    /// Must run before any threads are spawned: it may unshare the mount namespace.
    pub fn open(path: &Path) -> Result<Self> {
        let path = path.canonicalize().with_context(|| format!("cannot access {}", path.display()))?;
        let f = File::open(&path).with_context(|| format!("cannot open {}", path.display()))?;
        if fs_type(&f)? != BTRFS_SUPER_MAGIC {
            bail!("{} is not on a btrfs filesystem", path.display());
        }
        btrfs::ino_lookup(&f, 0, btrfs::FIRST_FREE_OBJECTID).map_err(root_hint)?;
        let info = fs::read_to_string("/proc/self/mountinfo")?;
        let device = mount_source(&info, &path)?;
        let mounted = mounted_subvols(&info, &device);
        let readonly = is_readonly(&path)?;
        // Always use a fresh mount of the top level: it has no submounts, so
        // nothing can lead into another filesystem, and it lives in a private
        // namespace in a directory only root can reach.
        let base = Path::new("/run");
        let base = if base.is_dir() { base.to_path_buf() } else { std::env::temp_dir() };
        let dir = make_mount_dir(&base)?;
        if let Err(e) = private_mount(&device, &dir, readonly) {
            let _ = fs::remove_dir(&dir);
            return Err(root_hint(e)).with_context(|| format!("mounting the top-level subvolume of {device}"));
        }
        let top = File::open(&dir)?;
        Ok(FsHandle { mount: dir, top, device, mounted })
    }
}

impl Drop for FsHandle {
    fn drop(&mut self) {
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

fn is_readonly(path: &Path) -> io::Result<bool> {
    let p = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: statvfs is plain data; statvfs() fills it.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(p.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(s.f_flag & libc::ST_RDONLY != 0)
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

fn private_mount(device: &str, dir: &Path, readonly: bool) -> io::Result<()> {
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
    let dev = CString::new(device)?;
    let target = CString::new(dir.as_os_str().as_bytes())?;
    // SAFETY: valid NUL-terminated strings.
    check(unsafe {
        let flags = if readonly { libc::MS_RDONLY } else { 0 };
        libc::mount(dev.as_ptr(), target.as_ptr(), c"btrfs".as_ptr(), flags, c"subvolid=5".as_ptr().cast())
    })
}

fn mount_source(info: &str, path: &Path) -> Result<String> {
    let mut best: Option<(usize, String)> = None;
    for m in parse_mountinfo(info) {
        let len = m.mountpoint.as_os_str().len();
        if m.fstype == "btrfs" && path.starts_with(&m.mountpoint) && best.as_ref().is_none_or(|b| len > b.0) {
            best = Some((len, m.source));
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

/// Creates a fresh, root-only directory to mount on; never reuses an existing one.
fn make_mount_dir(base: &Path) -> io::Result<PathBuf> {
    let mut rng = Rng::new(rng::time_seed());
    loop {
        let dir = base.join(format!("btdua-{}-{:016x}", std::process::id(), rng.next_u64()));
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

struct MountEntry {
    root: String,
    mountpoint: PathBuf,
    fstype: String,
    source: String,
}

fn parse_mountinfo(info: &str) -> Vec<MountEntry> {
    info.lines()
        .filter_map(|line| {
            let (pre, post) = line.split_once(" - ")?;
            let mut pre = pre.split(' ');
            let root = unescape(pre.nth(3)?);
            let mountpoint = PathBuf::from(unescape(pre.next()?));
            let mut post = post.split(' ');
            let fstype = post.next()?.to_string();
            let source = unescape(post.next()?);
            Some(MountEntry { root, mountpoint, fstype, source })
        })
        .collect()
}

/// Paths (relative to the top level) of `device` that are mounted anywhere.
fn mounted_subvols(mountinfo: &str, device: &str) -> Vec<String> {
    let mut v: Vec<String> = parse_mountinfo(mountinfo)
        .into_iter()
        .filter(|m| m.fstype == "btrfs" && m.source == device)
        .map(|m| m.root.trim_matches('/').to_string())
        .collect();
    v.sort();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn mount_dirs_are_unique_and_private() {
        let base = std::env::temp_dir();
        let a = super::make_mount_dir(&base).unwrap();
        let b = super::make_mount_dir(&base).unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::remove_dir(&a).unwrap();
        std::fs::remove_dir(&b).unwrap();
    }

    #[test]
    fn lists_mounted_subvolumes_of_device() {
        let info = "\
29 1 0:26 /@ / rw,noatime shared:1 - btrfs /dev/sde2 rw,subvolid=256,subvol=/@
31 29 0:26 /@home /home rw,noatime shared:2 - btrfs /dev/sde2 rw,subvolid=257,subvol=/@home
40 29 0:40 / /mnt/other rw shared:9 - btrfs /dev/sdb1 rw,subvolid=5,subvol=/
41 29 0:26 /@home/p4b/x /bind rw shared:3 - btrfs /dev/sde2 rw,subvolid=257,subvol=/@home
";
        assert_eq!(super::mounted_subvols(info, "/dev/sde2"), vec!["@", "@home", "@home/p4b/x"]);
    }

    #[test]
    fn unescapes_mountinfo() {
        assert_eq!(super::unescape(r"/mnt/my\040disk"), "/mnt/my disk");
        assert_eq!(super::unescape("/plain"), "/plain");
    }
}
