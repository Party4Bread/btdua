//! Thin wrappers over the btrfs ioctls btdua needs. Kernel structs are
//! encoded/decoded by byte offset so nothing depends on Rust layout.

mod ops;
mod search;

pub use ops::*;
pub use search::{SearchKey, tree_search};

use std::io;
use std::os::fd::AsRawFd;

const MAGIC: u64 = 0x94;
const IOC_W: u64 = 1;
const IOC_R: u64 = 2;
const IOC_RW: u64 = 3;

const fn ioc(dir: u64, nr: u64, size: u64) -> u64 {
    (dir << 30) | (size << 16) | (MAGIC << 8) | nr
}

pub(crate) const IOC_SNAP_DESTROY: u64 = ioc(IOC_W, 15, 4096);
pub(crate) const IOC_TREE_SEARCH_V2: u64 = ioc(IOC_RW, 17, 112);
pub(crate) const IOC_INO_LOOKUP: u64 = ioc(IOC_RW, 18, 4096);
pub(crate) const IOC_FS_INFO: u64 = ioc(IOC_R, 31, 1024);
pub(crate) const IOC_INO_PATHS: u64 = ioc(IOC_RW, 35, 56);
pub(crate) const IOC_LOGICAL_INO_V2: u64 = ioc(IOC_RW, 59, 56);

pub(crate) fn ioctl(fd: &impl AsRawFd, req: u64, arg: *mut u8) -> io::Result<()> {
    // SAFETY: callers pass a buffer at least as large as the ioctl's argument struct.
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), req as _, arg) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

pub(crate) fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
pub(crate) fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
pub(crate) fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
pub(crate) fn wr_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
pub(crate) fn wr_u64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

pub fn errno_name(e: &io::Error) -> String {
    let Some(n) = e.raw_os_error() else { return e.to_string() };
    let name = match n {
        libc::EPERM => "EPERM",
        libc::ENOENT => "ENOENT",
        libc::EIO => "EIO",
        libc::ENOMEM => "ENOMEM",
        libc::EACCES => "EACCES",
        libc::EINVAL => "EINVAL",
        libc::ENOTDIR => "ENOTDIR",
        libc::EOVERFLOW => "EOVERFLOW",
        libc::ENAMETOOLONG => "ENAMETOOLONG",
        libc::ELOOP => "ELOOP",
        libc::ESTALE => "ESTALE",
        libc::EUCLEAN => "EUCLEAN",
        _ => return format!("errno {n}"),
    };
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_kernel_headers() {
        assert_eq!(IOC_TREE_SEARCH_V2, 0xC070_9411);
        assert_eq!(IOC_INO_LOOKUP, 0xD000_9412);
        assert_eq!(IOC_INO_PATHS, 0xC038_9423);
        assert_eq!(IOC_LOGICAL_INO_V2, 0xC038_943B);
        assert_eq!(IOC_FS_INFO, 0x8400_941F);
        assert_eq!(IOC_SNAP_DESTROY, 0x5000_940F);
    }
}
