use std::fs::File;
use std::io;

use super::{IOC_TREE_SEARCH_V2, ioctl, rd_u32, rd_u64, wr_u32, wr_u64};

/// Key range for `BTRFS_IOC_TREE_SEARCH_V2`. The kernel compares
/// (objectid, type, offset) as a tuple, so callers must filter by type.
#[derive(Clone, Copy, Debug)]
pub struct SearchKey {
    pub tree_id: u64,
    pub min_objectid: u64,
    pub max_objectid: u64,
    pub min_type: u32,
    pub max_type: u32,
    pub min_offset: u64,
    pub max_offset: u64,
}

impl SearchKey {
    pub fn exact(tree_id: u64, objectid: u64, ty: u32) -> Self {
        SearchKey {
            tree_id,
            min_objectid: objectid,
            max_objectid: objectid,
            min_type: ty,
            max_type: ty,
            min_offset: 0,
            max_offset: u64::MAX,
        }
    }
}

pub struct Item<'a> {
    pub objectid: u64,
    pub ty: u32,
    pub offset: u64,
    pub data: &'a [u8],
}

const HDR: usize = 112;
const BUF: usize = 64 * 1024;

/// Visits every item in the key range; the callback returns `false` to stop.
pub fn tree_search(fd: &File, key: SearchKey, mut f: impl FnMut(&Item) -> bool) -> io::Result<()> {
    let mut buf = vec![0u8; HDR + BUF];
    let (mut obj, mut ty, mut off) = (key.min_objectid, key.min_type, key.min_offset);
    loop {
        buf[..HDR].fill(0);
        wr_u64(&mut buf, 0, key.tree_id);
        wr_u64(&mut buf, 8, obj);
        wr_u64(&mut buf, 16, key.max_objectid);
        wr_u64(&mut buf, 24, off);
        wr_u64(&mut buf, 32, key.max_offset);
        wr_u64(&mut buf, 40, 0);
        wr_u64(&mut buf, 48, u64::MAX);
        wr_u32(&mut buf, 56, ty);
        wr_u32(&mut buf, 60, key.max_type);
        wr_u32(&mut buf, 64, u32::MAX);
        wr_u64(&mut buf, 104, BUF as u64);
        ioctl(fd, IOC_TREE_SEARCH_V2, buf.as_mut_ptr())?;
        let n = rd_u32(&buf, 64);
        if n == 0 {
            return Ok(());
        }
        let mut p = HDR;
        let mut last = (0, 0, 0);
        for _ in 0..n {
            let objectid = rd_u64(&buf, p + 8);
            let offset = rd_u64(&buf, p + 16);
            let t = rd_u32(&buf, p + 24);
            let len = rd_u32(&buf, p + 28) as usize;
            let item = Item { objectid, ty: t, offset, data: &buf[p + 32..p + 32 + len] };
            if !f(&item) {
                return Ok(());
            }
            last = (objectid, t, offset);
            p += 32 + len;
        }
        let (o, t, of) = last;
        (obj, ty, off) = if of < u64::MAX {
            (o, t, of + 1)
        } else if t < u8::MAX as u32 {
            (o, t + 1, 0)
        } else if o < u64::MAX {
            (o + 1, 0, 0)
        } else {
            return Ok(());
        };
        if (obj, ty, off) > (key.max_objectid, key.max_type, key.max_offset) {
            return Ok(());
        }
    }
}
