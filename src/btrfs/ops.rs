use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;

use super::*;

pub const FIRST_FREE_OBJECTID: u64 = 256;
pub const FS_TREE_OBJECTID: u64 = 5;
const ROOT_TREE_OBJECTID: u64 = 1;
const CHUNK_TREE_OBJECTID: u64 = 3;
const FIRST_CHUNK_TREE_OBJECTID: u64 = 256;

const EXTENT_DATA_KEY: u32 = 108;
const ROOT_BACKREF_KEY: u32 = 144;
const CHUNK_ITEM_KEY: u32 = 228;

pub const BLOCK_GROUP_DATA: u64 = 1;
pub const BLOCK_GROUP_SYSTEM: u64 = 2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Chunk {
    pub start: u64,
    pub len: u64,
    pub flags: u64,
}

pub fn chunks(fd: &File) -> io::Result<Vec<Chunk>> {
    let mut v = Vec::new();
    let key = SearchKey::exact(CHUNK_TREE_OBJECTID, FIRST_CHUNK_TREE_OBJECTID, CHUNK_ITEM_KEY);
    tree_search(fd, key, |it| {
        if it.ty == CHUNK_ITEM_KEY && it.data.len() >= 32 {
            v.push(Chunk { start: it.offset, len: rd_u64(it.data, 0), flags: rd_u64(it.data, 24) });
        }
        true
    })?;
    Ok(v)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InodeRef {
    pub inum: u64,
    /// File offset of the resolved byte.
    pub offset: u64,
    pub root: u64,
}

const MAX_LOGICAL_BUF: usize = 16 << 20;

/// References covering exactly byte `logical`. `Err(ENOENT)` means no extent there.
pub fn logical_ino(fd: &File, logical: u64, buf: &mut Vec<u8>) -> io::Result<Vec<InodeRef>> {
    loop {
        let mut args = [0u8; 56];
        wr_u64(&mut args, 0, logical);
        wr_u64(&mut args, 8, buf.len() as u64);
        wr_u64(&mut args, 48, buf.as_mut_ptr() as u64);
        buf[..16].fill(0);
        ioctl(fd, IOC_LOGICAL_INO_V2, args.as_mut_ptr())?;
        let missing = rd_u32(buf, 4) as usize;
        if missing > 0 && buf.len() < MAX_LOGICAL_BUF {
            let want = (buf.len() + missing).next_power_of_two().min(MAX_LOGICAL_BUF);
            buf.resize(want, 0);
            continue;
        }
        let cnt = rd_u32(buf, 8) as usize;
        return Ok((0..cnt / 3)
            .map(|i| {
                let p = 16 + i * 24;
                InodeRef { inum: rd_u64(buf, p), offset: rd_u64(buf, p + 8), root: rd_u64(buf, p + 16) }
            })
            .collect());
    }
}

/// Paths of `inum` relative to the subvolume that `fd` lives in.
pub fn ino_paths(fd: &File, inum: u64) -> io::Result<Vec<String>> {
    let mut buf = vec![0u8; 4096];
    let mut args = [0u8; 56];
    wr_u64(&mut args, 0, inum);
    wr_u64(&mut args, 8, buf.len() as u64);
    wr_u64(&mut args, 48, buf.as_mut_ptr() as u64);
    ioctl(fd, IOC_INO_PATHS, args.as_mut_ptr())?;
    Ok(decode_paths(&buf))
}

fn decode_paths(buf: &[u8]) -> Vec<String> {
    let cnt = rd_u32(buf, 8) as usize;
    let mut out = Vec::with_capacity(cnt);
    for i in 0..cnt {
        let slot = 16 + i * 8;
        if slot + 8 > buf.len() {
            break;
        }
        let off = 16 + rd_u64(buf, slot) as usize;
        if off >= buf.len() {
            break;
        }
        let end = buf[off..].iter().position(|&b| b == 0).map_or(buf.len(), |e| off + e);
        out.push(String::from_utf8_lossy(&buf[off..end]).into_owned());
    }
    out
}

/// With `treeid == 0` the kernel uses the root `fd` belongs to. Returns the
/// tree id and the directory path of `objectid` inside it ("a/b/" or "").
pub fn ino_lookup(fd: &File, treeid: u64, objectid: u64) -> io::Result<(u64, String)> {
    let mut a = vec![0u8; 4096];
    wr_u64(&mut a, 0, treeid);
    wr_u64(&mut a, 8, objectid);
    ioctl(fd, IOC_INO_LOOKUP, a.as_mut_ptr())?;
    let name = &a[16..];
    let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    Ok((rd_u64(&a, 0), String::from_utf8_lossy(&name[..end]).into_owned()))
}

/// (parent root, directory inode in parent, entry name) for subvolume `root`.
pub fn root_backref(fd: &File, root: u64) -> io::Result<Option<(u64, u64, String)>> {
    let mut r = None;
    tree_search(fd, SearchKey::exact(ROOT_TREE_OBJECTID, root, ROOT_BACKREF_KEY), |it| {
        if it.ty != ROOT_BACKREF_KEY || it.data.len() < 18 {
            return true;
        }
        let n = (rd_u16(it.data, 16) as usize).min(it.data.len() - 18);
        let name = String::from_utf8_lossy(&it.data[18..18 + n]).into_owned();
        r = Some((it.offset, rd_u64(it.data, 0), name));
        false
    })?;
    Ok(r)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FileExtent {
    pub file_offset: u64,
    /// 0 inline, 1 regular, 2 prealloc.
    pub kind: u8,
    /// 0 none, 1 zlib, 2 lzo, 3 zstd.
    pub compression: u8,
    pub ram_bytes: u64,
    pub disk_bytenr: u64,
    pub disk_num_bytes: u64,
    pub num_bytes: u64,
}

pub(crate) fn decode_file_extent(file_offset: u64, d: &[u8]) -> Option<FileExtent> {
    if d.len() < 21 {
        return None;
    }
    let ram_bytes = rd_u64(d, 8);
    let kind = d[20];
    let mut fe = FileExtent {
        file_offset,
        kind,
        compression: d[16],
        ram_bytes,
        disk_bytenr: 0,
        disk_num_bytes: 0,
        num_bytes: ram_bytes,
    };
    if kind != 0 {
        if d.len() < 53 {
            return None;
        }
        fe.disk_bytenr = rd_u64(d, 21);
        fe.disk_num_bytes = rd_u64(d, 29);
        fe.num_bytes = rd_u64(d, 45);
    }
    Some(fe)
}

pub fn file_extents(
    fd: &File,
    root: u64,
    inum: u64,
    min_off: u64,
    max_off: u64,
    mut f: impl FnMut(&FileExtent) -> bool,
) -> io::Result<()> {
    let mut key = SearchKey::exact(root, inum, EXTENT_DATA_KEY);
    key.min_offset = min_off;
    key.max_offset = max_off;
    tree_search(fd, key, |it| {
        if it.ty != EXTENT_DATA_KEY || it.objectid != inum {
            return true;
        }
        match decode_file_extent(it.offset, it.data) {
            Some(fe) => f(&fe),
            None => true,
        }
    })
}

pub fn fsid(fd: &File) -> io::Result<[u8; 16]> {
    let mut a = vec![0u8; 1024];
    ioctl(fd, IOC_FS_INFO, a.as_mut_ptr())?;
    Ok(a[16..32].try_into().unwrap())
}

pub fn fsid_string(id: &[u8; 16]) -> String {
    let h: String = id.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Destroys subvolume `name` inside directory `parent` (must have no nested subvolumes).
pub fn snap_destroy(parent: &File, name: &OsStr) -> io::Result<()> {
    let b = name.as_bytes();
    if b.len() >= 4088 {
        return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
    }
    let mut a = vec![0u8; 4096];
    a[8..8 + b.len()].copy_from_slice(b);
    ioctl(parent, IOC_SNAP_DESTROY, a.as_mut_ptr())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_regular_compressed_extent() {
        let mut d = vec![0u8; 53];
        wr_u64(&mut d, 8, 131072); // ram_bytes
        d[16] = 3; // zstd
        d[20] = 1; // regular
        wr_u64(&mut d, 21, 0x1000_0000);
        wr_u64(&mut d, 29, 16384);
        wr_u64(&mut d, 45, 131072);
        let fe = decode_file_extent(4096, &d).unwrap();
        assert_eq!(fe.compression, 3);
        assert_eq!((fe.disk_bytenr, fe.disk_num_bytes, fe.ram_bytes), (0x1000_0000, 16384, 131072));
    }

    #[test]
    fn decodes_ino_paths_container() {
        let mut b = vec![0u8; 64];
        wr_u32(&mut b, 8, 2); // elem_cnt
        wr_u64(&mut b, 16, 16); // offsets relative to val[0] (byte 16)
        wr_u64(&mut b, 24, 20);
        b[32..36].copy_from_slice(b"a/b\0");
        b[36..38].copy_from_slice(b"c\0");
        assert_eq!(decode_paths(&b), vec!["a/b".to_string(), "c".to_string()]);
    }

    #[test]
    fn formats_fsid() {
        let id = [0x12u8; 16];
        assert_eq!(fsid_string(&id), "12121212-1212-1212-1212-121212121212");
    }
}
