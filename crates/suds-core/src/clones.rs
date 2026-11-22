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
    let mut request = map::Request::default();
    loop {
        match map::read(&file, &mut request) {
            Ok(()) => {}
            // The filesystem offers no map, so no file of it counts less.
            Err(Errno::OPNOTSUPP | Errno::NOTTY | Errno::INVAL) => return Ok(Vec::new()),
            Err(errno) => return Err(errno.into()),
        }
        let mapped = request.mapped();
        let Some(last) = mapped.last() else {
            return Ok(extents);
        };
        extents.extend(
            mapped
                .iter()
                .filter(|extent| map::counts(extent.flags))
                .map(|extent| (extent.physical, extent.length)),
        );
        if last.flags & map::LAST != 0 {
            return Ok(extents);
        }
        request.start = last.logical.saturating_add(last.length);
    }
}

/// The kernel's map of a file's extents, which Linux names FIEMAP.
#[cfg(target_os = "linux")]
mod map {
    use std::os::fd::OwnedFd;

    use rustix::io::Errno;
    use rustix::ioctl::{Updater, ioctl};

    /// The request's number, which the kernel's headers name
    /// `FS_IOC_FIEMAP`.
    const OPCODE: rustix::ioctl::Opcode = 0xC020_660B;

    /// How many extents one request reads.
    const ROOM: usize = 64;

    /// The file's last extent.
    pub const LAST: u32 = 0x1;
    /// The extent's place on the disk is unknown.
    const UNKNOWN: u32 = 0x2;
    /// The filesystem compressed or encrypted the extent.
    const ENCODED: u32 = 0x8;
    /// Another file holds the extent too.
    const SHARED: u32 = 0x2000;

    /// One extent, in the kernel's layout of `struct fiemap_extent`.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct Mapped {
        pub logical: u64,
        pub physical: u64,
        pub length: u64,
        reserved_wide: [u64; 2],
        pub flags: u32,
        reserved: [u32; 3],
    }

    /// One request, in the kernel's layout of `struct fiemap` with room
    /// for its extents behind the header.
    #[repr(C)]
    pub struct Request {
        /// The byte of the file at which the map begins.
        pub start: u64,
        length: u64,
        flags: u32,
        mapped: u32,
        room: u32,
        reserved: u32,
        extents: [Mapped; ROOM],
    }

    impl Default for Request {
        fn default() -> Self {
            Request {
                start: 0,
                length: u64::MAX,
                // Without the flag of a sync, the request writes no data.
                flags: 0,
                mapped: 0,
                room: ROOM as u32,
                reserved: 0,
                extents: [Mapped::default(); ROOM],
            }
        }
    }

    impl Request {
        /// The extents that the last reading filled.
        pub fn mapped(&self) -> &[Mapped] {
            &self.extents[..(self.mapped as usize).min(ROOM)]
        }
    }

    /// Reads the extents of `file` from the request's start.
    #[expect(
        unsafe_code,
        reason = "rustix offers the kernel's extent map through its unsafe ioctl alone"
    )]
    pub fn read(file: &OwnedFd, request: &mut Request) -> Result<(), Errno> {
        request.mapped = 0;
        // SAFETY: the opcode is FIEMAP's. The kernel reads the header of
        // `struct fiemap` and writes at most `room` extents behind it, and
        // `Request` holds that header and that many extents in the
        // kernel's layout.
        unsafe { ioctl(file, Updater::<OPCODE, Request>::new(request)) }
    }

    /// Whether an extent with `flags` counts as shared. An encoded extent
    /// states its length before compression, which exceeds the blocks that
    /// it occupies, so the scan leaves it in each file.
    pub fn counts(flags: u32) -> bool {
        flags & SHARED != 0 && flags & (ENCODED | UNKNOWN) == 0
    }

    #[cfg(test)]
    mod tests {
        use super::{ENCODED, LAST, Mapped, Request, SHARED, UNKNOWN, counts};

        #[test]
        fn leaves_an_encoded_extent_in_each_file() {
            assert!(counts(SHARED));
            assert!(counts(SHARED | LAST));
            assert!(!counts(LAST));
            assert!(!counts(SHARED | ENCODED));
            assert!(!counts(SHARED | UNKNOWN));
        }

        #[test]
        fn a_request_holds_the_kernels_layout() {
            assert_eq!(size_of::<Mapped>(), 56);
            assert_eq!(size_of::<Request>(), 32 + 64 * 56);
            assert!(Request::default().mapped().is_empty());
        }
    }
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
}
