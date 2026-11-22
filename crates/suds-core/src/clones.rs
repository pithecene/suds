//! The extents that a file shares with another file.
//!
//! A filesystem that clones files, such as btrfs or XFS, stores a cloned
//! block once for each file that holds it. A measurement that sums each
//! file's blocks counts such a block once for each file. The kernel names
//! each shared extent of a file, so a scan counts the extent in the first
//! file that reaches it, as it counts a file with several links.

use std::collections::{BTreeMap, HashMap};
use std::fs::Metadata;
use std::io;
use std::path::Path;

/// One extent on the disk: its first byte and its length in bytes.
pub(crate) type Extent = (u64, u64);

/// The ranges of each filesystem's disk that a scan has counted.
#[derive(Default)]
pub(crate) struct Counted {
    /// For each device, each counted range by its first byte, with the
    /// byte after its last. No two ranges of a device touch.
    devices: HashMap<u64, BTreeMap<u64, u64>>,
}

impl Counted {
    /// Counts `extent` on `device`, and returns how many of its bytes the
    /// scan counted before. Two clones can name one block through extents
    /// of different bounds. A device names one subvolume of btrfs.
    pub(crate) fn claim(&mut self, device: u64, extent: Extent) -> u64 {
        let (mut start, length) = extent;
        let mut end = start.saturating_add(length);
        let ranges = self.devices.entry(device).or_default();
        let mut before = 0_u64;
        let touching: Vec<(u64, u64)> = ranges
            .range(..=end)
            .rev()
            .take_while(|(_, range_end)| **range_end >= start)
            .map(|(range_start, range_end)| (*range_start, *range_end))
            .collect();
        for (range_start, range_end) in touching {
            let shared = range_end.min(end).saturating_sub(range_start.max(start));
            before = before.saturating_add(shared);
            start = start.min(range_start);
            end = end.max(range_end);
            ranges.remove(&range_start);
        }
        ranges.insert(start, end);
        before
    }
}

/// The size below which a scan asks for no file's extents. The question
/// costs one open for each file, and small files hold few shared bytes.
/// On one workstation the floor kept 89 percent of the correction. It
/// cost a seventh of the time.
#[cfg(target_os = "linux")]
const FLOOR: u64 = 16 * 1024;

/// Each extent of the regular file at `path` that the file shares, in the
/// file's order. A file below the floor yields none, and so does a
/// filesystem that maps no file. An error states that Suds could not open
/// or map the file, whose blocks then count in full.
#[cfg(target_os = "linux")]
pub(crate) fn shared(path: &Path, meta: &Metadata) -> io::Result<Vec<Extent>> {
    use std::os::unix::fs::MetadataExt;

    use fiemap::Fiemap;
    use rustix::fs::{Mode, OFlags, open};
    use rustix::io::Errno;

    if meta.blocks() == 0 || meta.len() < FLOOR {
        return Ok(Vec::new());
    }
    // Another entry may have replaced the file. The open follows no link
    // and waits on no pipe.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let file = open(path, flags, Mode::empty())?;
    let mut extents = Vec::new();
    for extent in Fiemap::new(file) {
        match extent {
            Ok(extent) => {
                if counts(extent.fe_flags) {
                    extents.push((extent.fe_physical, extent.fe_length));
                }
            }
            // The filesystem offers no map, so no file of it counts less.
            Err(err)
                if [Errno::OPNOTSUPP, Errno::NOTTY, Errno::INVAL]
                    .iter()
                    .any(|none| err.raw_os_error() == Some(none.raw_os_error())) =>
            {
                return Ok(Vec::new());
            }
            Err(err) => return Err(err),
        }
    }
    Ok(extents)
}

/// Whether an extent with `flags` counts as shared. An encoded extent
/// states its length before compression, which exceeds the blocks that it
/// occupies, so the scan leaves it in each file.
#[cfg(target_os = "linux")]
fn counts(flags: fiemap::FiemapExtentFlags) -> bool {
    use fiemap::FiemapExtentFlags as Flags;

    flags.contains(Flags::SHARED) && !flags.intersects(Flags::ENCODED | Flags::UNKNOWN)
}

/// No other system names a shared extent to a scan.
#[cfg(not(target_os = "linux"))]
pub(crate) fn shared(_path: &Path, _meta: &Metadata) -> io::Result<Vec<Extent>> {
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::Counted;

    #[test]
    fn counts_each_byte_of_a_device_once_whatever_the_extents_bounds() {
        let mut counted = Counted::default();
        assert_eq!(counted.claim(1, (1000, 300)), 0);
        // The same extent again, and a part of it under other bounds.
        assert_eq!(counted.claim(1, (1000, 300)), 300);
        assert_eq!(counted.claim(1, (1100, 50)), 50);
        // An extent that begins before the range and ends within it.
        assert_eq!(counted.claim(1, (900, 150)), 50);
        // An extent that spans two ranges and the gap between them.
        assert_eq!(counted.claim(1, (2000, 100)), 0);
        assert_eq!(counted.claim(1, (1200, 850)), 150);
        assert_eq!(counted.claim(1, (900, 1200)), 1200);
        // A neighbour shares no byte.
        assert_eq!(counted.claim(1, (2100, 10)), 0);
        // Another filesystem numbers its own bytes.
        assert_eq!(counted.claim(2, (1000, 300)), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn leaves_an_encoded_extent_in_each_file() {
        use fiemap::FiemapExtentFlags as Flags;

        use super::counts;

        assert!(counts(Flags::SHARED));
        assert!(counts(Flags::SHARED | Flags::LAST));
        assert!(!counts(Flags::LAST));
        assert!(!counts(Flags::SHARED | Flags::ENCODED));
        assert!(!counts(Flags::SHARED | Flags::UNKNOWN | Flags::DELALLOC));
    }
}
