//! Filesystem-changing and expensive operations run off the UI thread.

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
