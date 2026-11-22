//! The extents that a file shares with another file.
//!
//! A filesystem that clones files, such as btrfs or XFS, stores a cloned
//! block once for each file that holds it. A measurement that sums each
//! file's blocks counts such a block once for each file. The kernel names
//! each shared extent of a file, so a scan counts the extent in the first
//! file that reaches it, as it counts a file with several links.

use std::fs::Metadata;
use std::path::Path;

/// One extent on the disk: its first byte and its length in bytes.
pub(crate) type Extent = (u64, u64);

/// The size below which a scan asks for no file's extents. The question
/// costs one open for each file, and small files hold few shared bytes.
/// On one workstation the floor kept 89 percent of the correction. It
/// cost a seventh of the time.
#[cfg(target_os = "linux")]
const FLOOR: u64 = 16 * 1024;

/// Each extent of the regular file at `path` that the file shares, in the
/// file's order. A file below the floor yields none, and so does a file
/// that Suds cannot open or map, so its blocks count in full.
#[cfg(target_os = "linux")]
pub(crate) fn shared(path: &Path, meta: &Metadata) -> Vec<Extent> {
    use std::os::unix::fs::MetadataExt;

    use fiemap::{Fiemap, FiemapExtentFlags};
    use rustix::fs::{Mode, OFlags, open};

    if meta.blocks() == 0 || meta.len() < FLOOR {
        return Vec::new();
    }
    // Another entry may have replaced the file. The open follows no link
    // and waits on no pipe.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let Ok(file) = open(path, flags, Mode::empty()) else {
        return Vec::new();
    };
    let mut extents = Vec::new();
    for extent in Fiemap::new(file) {
        let Ok(extent) = extent else {
            return Vec::new();
        };
        if extent.fe_flags.contains(FiemapExtentFlags::SHARED) {
            extents.push((extent.fe_physical, extent.fe_length));
        }
    }
    extents
}

/// No other system names a shared extent to a scan.
#[cfg(not(target_os = "linux"))]
pub(crate) fn shared(_path: &Path, _meta: &Metadata) -> Vec<Extent> {
    Vec::new()
}
