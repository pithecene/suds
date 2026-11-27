//! The extents that a file shares with another file.
//!
//! A filesystem that clones files, such as btrfs, XFS, or APFS, stores a cloned
//! block once for each file that holds it. A measurement that sums each
//! file's blocks counts such a block once for each file. The kernel names
//! each shared extent of a file, so a scan counts the extent in the first
//! file that reaches it, as it counts a file with several links. APFS names
//! the file's clones in place of its extents.

use std::collections::{BTreeMap, HashMap};
use std::fs::Metadata;
use std::io;
use std::path::Path;

/// One extent on the disk: its first byte and its length in bytes. On macOS,
/// the number of a file's clones stands in for the place on the disk.
pub(crate) type Extent = (u64, u64);

/// The ranges of each filesystem's disk that a scan has counted.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
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

    /// Returns the sub-extents of `extent` on `device` not yet covered by any
    /// counted range, without mutating `self`.
    pub(crate) fn uncovered(&self, device: u64, extent: Extent) -> Vec<Extent> {
        let (start, length) = extent;
        if length == 0 {
            return Vec::new();
        }
        let end = start.saturating_add(length);
        let Some(ranges) = self.devices.get(&device) else {
            return vec![extent];
        };
        let mut cursor = start;
        let mut result = Vec::new();
        if let Some((_, &range_end)) = ranges.range(..=start).next_back()
            && range_end > start
        {
            cursor = range_end.min(end);
        }
        for (&range_start, &range_end) in ranges.range(start..end) {
            if range_start > cursor {
                result.push((cursor, range_start - cursor));
            }
            if range_end > cursor {
                cursor = range_end.min(end);
            }
            if cursor >= end {
                break;
            }
        }
        if cursor < end {
            result.push((cursor, end - cursor));
        }
        result
    }

    /// Merges all counted ranges from `other` into `self`.
    pub(crate) fn merge(&mut self, other: Counted) {
        for (device, ranges) in other.devices {
            for (start, end) in ranges {
                let length = end.saturating_sub(start);
                if length > 0 {
                    self.claim(device, (start, length));
                }
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        self.devices.clear();
    }

    /// Exports counted ranges in deterministic device and start-offset order.
    pub(crate) fn to_device_extents(&self) -> Vec<crate::state::DeviceExtents> {
        let mut devices: Vec<crate::state::DeviceExtents> = self
            .devices
            .iter()
            .filter(|(_, ranges)| !ranges.is_empty())
            .map(|(&device, ranges)| crate::state::DeviceExtents {
                device,
                ranges: ranges.iter().map(|(&s, &e)| (s, e)).collect(),
            })
            .collect();
        devices.sort_by_key(|entry| entry.device);
        devices
    }

    /// Reconstructs counted ranges from persisted `(start, end)` intervals.
    pub(crate) fn from_device_extents(data: &[crate::state::DeviceExtents]) -> Self {
        let mut counted = Counted::default();
        for entry in data {
            for &(start, end) in &entry.ranges {
                let length = end.saturating_sub(start);
                if length > 0 {
                    counted.claim(entry.device, (start, length));
                }
            }
        }
        counted
    }
}

/// The size below which a scan asks for no file's clones. Each question costs
/// one call, and small files hold few shared bytes. The floor kept 89 percent
/// of the correction on one Linux workstation, which cost a seventh of the
/// time. It kept 97 percent on one macOS workstation, at a fourteenth.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) const FLOOR: u64 = 16 * 1024;

#[cfg(target_os = "macos")]
pub(crate) const SHARES_ALL: u64 = apfs::SHARES_ALL;

#[cfg(target_os = "macos")]
pub(crate) fn apfs_extent(number: u64, bytes: u64) -> Option<Extent> {
    apfs::extent(number, bytes)
}

#[cfg(target_os = "macos")]
pub(crate) fn query_dir_bulk(
    fd: &rustix::fd::OwnedFd,
    request: &mut libc::attrlist,
    buf: &mut [u8],
) -> io::Result<i32> {
    apfs::query_attrs(apfs::AttrTarget::DirFd(fd), request, buf)
}

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

/// The data of the regular file at `path`, as one extent, where it shares
/// every block with a clone. APFS gives a file that changes another number.
/// A file below the floor counts in full.
#[cfg(target_os = "macos")]
pub(crate) fn shared(path: &Path, meta: &Metadata) -> io::Result<Vec<Extent>> {
    use std::os::unix::fs::MetadataExt;

    if meta.blocks() == 0 || meta.len() < FLOOR {
        return Ok(Vec::new());
    }
    let Some(clones) = apfs::clones(path)? else {
        return Ok(Vec::new());
    };
    // Another entry may have taken the path since the scan read `meta`.
    if (clones.device, clones.inode) != (meta.dev(), meta.ino()) {
        return Ok(Vec::new());
    }
    let bytes = clones.bytes.min(meta.blocks() * 512);
    Ok(apfs::extent(clones.number, bytes).into_iter().collect())
}

/// The attributes by which APFS names a file's clones.
#[cfg(target_os = "macos")]
mod apfs {
    use std::ffi::{CStr, CString};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use rustix::fd::OwnedFd;

    use super::Extent;

    /// The file shares every block with another file, which the system's
    /// headers name `EF_SHARES_ALL_BLOCKS`.
    pub(super) const SHARES_ALL: u64 = 0x40;
    /// The range of places that each number owns, so no two numbers' extents
    /// touch.
    const SPAN: u64 = 1 << 32;
    /// Size of the kernel's packed single-file answer buffer.
    const ANSWER_LEN: usize = 4 + 20 + 4 + 8 + 8 + 8 + 8;

    /// Target for the single macOS APFS attribute syscall boundary.
    pub(super) enum AttrTarget<'a> {
        Path(&'a CStr),
        DirFd(&'a OwnedFd),
    }

    /// A file whose data shares every block with another file.
    pub struct Clones {
        pub device: u64,
        pub inode: u64,
        /// The number that the file shares with its clones.
        pub number: u64,
        /// The allocated bytes of the data alone. A resource fork is no part
        /// of what the number names.
        pub bytes: u64,
    }

    /// Queries macOS APFS attributes for either one path (`getattrlist`) or
    /// one open directory descriptor (`getattrlistbulk`).
    #[expect(
        unsafe_code,
        reason = "no safe interface reads the clone attributes of APFS"
    )]
    pub(super) fn query_attrs(
        target: AttrTarget<'_>,
        request: &mut libc::attrlist,
        buf: &mut [u8],
    ) -> io::Result<i32> {
        // SAFETY: `request` holds a valid `attrlist`, `buf` is writable for
        // `buf.len()` bytes, and the target is either a NUL-terminated path
        // or an open directory descriptor.
        let status = match target {
            AttrTarget::Path(path) => unsafe {
                libc::getattrlist(
                    path.as_ptr(),
                    (&raw mut *request).cast(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::FSOPT_ATTR_CMN_EXTENDED | libc::FSOPT_NOFOLLOW,
                )
            },
            AttrTarget::DirFd(fd) => unsafe {
                libc::getattrlistbulk(
                    fd.as_raw_fd(),
                    (&raw mut *request).cast(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    u64::from(libc::FSOPT_ATTR_CMN_EXTENDED),
                )
            },
        };
        if status < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(status)
        }
    }

    fn read_u32(bytes: &[u8], offset: usize) -> u32 {
        u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn read_i32(bytes: &[u8], offset: usize) -> i32 {
        i32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn read_u64(bytes: &[u8], offset: usize) -> u64 {
        u64::from_ne_bytes(bytes[offset..offset + 8].try_into().unwrap())
    }

    fn read_i64(bytes: &[u8], offset: usize) -> i64 {
        i64::from_ne_bytes(bytes[offset..offset + 8].try_into().unwrap())
    }

    /// The clones of the file at `path`, or none where the file shares less
    /// than every block or the filesystem names no clones.
    pub fn clones(path: &Path) -> io::Result<Option<Clones>> {
        let path = CString::new(path.as_os_str().as_bytes())?;
        let mut request = libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: libc::ATTR_CMN_RETURNED_ATTRS
                | libc::ATTR_CMN_DEVID
                | libc::ATTR_CMN_FILEID,
            volattr: 0,
            dirattr: 0,
            fileattr: libc::ATTR_FILE_DATAALLOCSIZE,
            forkattr: libc::ATTR_CMNEXT_CLONEID | libc::ATTR_CMNEXT_EXT_FLAGS,
        };
        let mut buf = [0_u8; ANSWER_LEN];
        if let Err(err) = query_attrs(AttrTarget::Path(&path), &mut request, &mut buf) {
            // A filesystem without these attributes names no clone.
            return match err.raw_os_error() {
                Some(libc::EINVAL | libc::ENOTSUP) => Ok(None),
                _ => Err(err),
            };
        }
        let returned_common = read_u32(&buf, 4);
        let returned_file = read_u32(&buf, 16);
        let returned_fork = read_u32(&buf, 20);
        // An attribute that the filesystem leaves out moves those behind it.
        let complete = returned_common == request.commonattr
            && returned_file == request.fileattr
            && returned_fork == request.forkattr;
        let flags = read_u64(&buf, 52);
        if !complete || flags & SHARES_ALL == 0 {
            return Ok(None);
        }
        Ok(Some(Clones {
            device: u64::from(read_i32(&buf, 24).cast_unsigned()),
            inode: read_u64(&buf, 28),
            number: read_u64(&buf, 44),
            bytes: u64::try_from(read_i64(&buf, 36)).unwrap_or(0),
        }))
    }

    /// The extent that stands for `bytes` of the clones `number`, or none
    /// where the two exceed the range of places.
    pub fn extent(number: u64, bytes: u64) -> Option<Extent> {
        (bytes < SPAN).then_some(())?;
        Some((number.checked_mul(SPAN)?, bytes))
    }

    #[cfg(test)]
    mod tests {
        use super::{ANSWER_LEN, extent};

        #[test]
        fn the_answer_holds_the_kernels_layout() {
            assert_eq!(ANSWER_LEN, 4 + 20 + 4 + 8 + 8 + 8 + 8);
        }

        #[test]
        fn two_numbers_own_ranges_apart() {
            let (first, length) = extent(7, (1 << 32) - 1).unwrap();
            assert!(first + length < extent(8, 1).unwrap().0);
            assert_eq!(extent(7, 1 << 32), None);
            assert_eq!(extent(1 << 33, 1), None);
        }
    }
}

/// No other system names a shared extent to a scan.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

    #[test]
    fn uncovered_merge_and_serialization_preserve_intervals() {
        let mut base = Counted::default();
        assert_eq!(base.claim(1, (100, 100)), 0);
        assert_eq!(base.claim(1, (300, 100)), 0);

        assert_eq!(base.uncovered(1, (150, 200)), vec![(200, 100)]);
        assert_eq!(base.uncovered(1, (100, 100)), Vec::<(u64, u64)>::new());
        assert_eq!(base.uncovered(1, (50, 200)), vec![(50, 50), (200, 50)]);
        assert_eq!(base.uncovered(2, (100, 50)), vec![(100, 50)]);

        let mut staged = Counted::default();
        assert_eq!(staged.claim(1, (200, 100)), 0);
        base.merge(staged);
        assert_eq!(base.uncovered(1, (100, 300)), Vec::<(u64, u64)>::new());

        let roundtrip = Counted::from_device_extents(&base.to_device_extents());
        assert_eq!(roundtrip, base);
    }
}
