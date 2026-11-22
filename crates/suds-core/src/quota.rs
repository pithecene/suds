//! The limit that a quota group of btrfs places on a subvolume.
//!
//! The kernel publishes each quota group beneath `/sys/fs/btrfs`, where any
//! user reads it, so Suds reads a limit without a privilege.

use std::path::Path;

/// The limit of the subvolume that holds a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quota {
    /// The most that the subvolume may reference, in bytes.
    pub limit_bytes: u64,
    /// What the subvolume references now, in bytes.
    pub referenced_bytes: u64,
}

/// The limit on the subvolume that holds `path`, or none where no quota
/// group limits it or the kernel names no subvolume.
#[cfg(target_os = "linux")]
pub fn of(path: &Path) -> Option<Quota> {
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let device = linux::device(&mounts, path)?;
    let subvolume = linux::subvolume(path)?;
    linux::read(Path::new("/sys/fs/btrfs"), &device, subvolume)
}

/// No other system holds a quota group.
#[cfg(not(target_os = "linux"))]
pub fn of(_path: &Path) -> Option<Quota> {
    None
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};

    use rustix::fs::{AtFlags, CWD, StatxFlags, statx};

    use super::Quota;

    /// The bit of `statx` that asks for a subvolume, which Linux 6.10 added.
    const STATX_SUBVOL: u32 = 0x8000;

    /// The flag of a quota group that holds a limit on referenced bytes.
    const LIMIT_REFERENCED: u64 = 1;

    /// The subvolume that holds `path`, by its identifier.
    pub fn subvolume(path: &Path) -> Option<u64> {
        let stat = statx(
            CWD,
            path,
            AtFlags::SYMLINK_NOFOLLOW,
            StatxFlags::from_bits_retain(STATX_SUBVOL),
        )
        .ok()?;
        (stat.stx_mask & STATX_SUBVOL != 0).then_some(stat.stx_subvol)
    }

    /// The device of the btrfs mount that holds `path`, by the name that
    /// the kernel lists it under. The text is the process's mount table.
    pub fn device(mounts: &str, path: &Path) -> Option<OsString> {
        let source = mounts
            .lines()
            .filter_map(mount)
            .filter(|(point, _)| path.starts_with(point))
            .max_by_key(|(point, _)| point.as_os_str().len())
            .and_then(|(_, source)| source)?;
        // A mapped device has a name of its own behind a link.
        let source = fs::canonicalize(&source).unwrap_or(source);
        source.file_name().map(OsString::from)
    }

    /// One line of the mount table: its mount point, and its device where
    /// the filesystem is btrfs.
    fn mount(line: &str) -> Option<(PathBuf, Option<PathBuf>)> {
        let (before, after) = line.split_once(" - ")?;
        let point = PathBuf::from(unescape(before.split(' ').nth(4)?));
        let mut fields = after.split(' ');
        let source = (fields.next()? == "btrfs")
            .then(|| fields.next().map(|source| PathBuf::from(unescape(source))))
            .flatten();
        Some((point, source))
    }

    /// The table writes a space, a tab, a line feed, and a backslash as a
    /// backslash and three octal digits.
    fn unescape(field: &str) -> String {
        let mut text = String::new();
        let mut rest = field;
        while let Some(at) = rest.find('\\') {
            text.push_str(&rest[..at]);
            let code = rest.get(at + 1..at + 4);
            match code.and_then(|digits| u8::from_str_radix(digits, 8).ok()) {
                Some(byte) => {
                    text.push(char::from(byte));
                    rest = &rest[at + 4..];
                }
                None => {
                    text.push('\\');
                    rest = &rest[at + 1..];
                }
            }
        }
        text.push_str(rest);
        text
    }

    /// Reads the quota group of `subvolume` on the filesystem that lists
    /// `device`, beneath the kernel's directory `published`.
    pub fn read(published: &Path, device: &OsString, subvolume: u64) -> Option<Quota> {
        let filesystem = fs::read_dir(published)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .find(|filesystem| filesystem.join("devices").join(device).exists())?;
        let group = filesystem.join("qgroups").join(format!("0_{subvolume}"));
        let number = |name: &str| -> Option<u64> {
            fs::read_to_string(group.join(name)).ok()?.trim().parse().ok()
        };
        if number("limit_flags")? & LIMIT_REFERENCED == 0 {
            return None;
        }
        Some(Quota {
            limit_bytes: number("max_referenced")?,
            referenced_bytes: number("referenced")?,
        })
    }

    #[cfg(test)]
    mod tests {
        use std::ffi::OsString;
        use std::fs;
        use std::path::Path;

        use tempfile::TempDir;

        use super::{Quota, device, read};

        const MOUNTS: &str = "\
57 45 0:45 /@/home /home rw,relatime shared:47 - btrfs /dev/nvme0n1p2 rw,subvolid=262,subvol=/@/home
61 45 0:41 /@ / rw,relatime shared:1 - btrfs /dev/nvme0n1p2 rw,subvolid=364,subvol=/@
70 57 0:60 / /home/a/my\\040disk rw - btrfs /dev/sdb1 rw
71 57 0:61 / /home/a/other rw - ext4 /dev/sdc1 rw
";

        #[test]
        fn names_the_device_of_the_deepest_btrfs_mount() {
            let name = |path: &str| device(MOUNTS, Path::new(path));
            assert_eq!(name("/home/a/work"), Some(OsString::from("nvme0n1p2")));
            assert_eq!(name("/usr"), Some(OsString::from("nvme0n1p2")));
            assert_eq!(name("/home/a/my disk/tree"), Some(OsString::from("sdb1")));
            // The deepest mount holds another filesystem, so no group limits it.
            assert_eq!(name("/home/a/other/tree"), None);
        }

        #[test]
        fn reads_a_limit_on_referenced_bytes_and_no_other() {
            let dir = TempDir::new().unwrap();
            let filesystem = dir.path().join("0723c641");
            fs::create_dir_all(filesystem.join("devices/sdb1")).unwrap();
            let group = |subvolume: u64, flags: &str, limit: &str| {
                let group = filesystem.join(format!("qgroups/0_{subvolume}"));
                fs::create_dir_all(&group).unwrap();
                fs::write(group.join("limit_flags"), flags).unwrap();
                fs::write(group.join("max_referenced"), limit).unwrap();
                fs::write(group.join("referenced"), "4096\n").unwrap();
            };
            group(256, "1\n", "1073741824\n");
            group(257, "0\n", "0\n");
            // A limit on exclusive bytes bounds no shared block.
            group(258, "2\n", "0\n");
            let device = OsString::from("sdb1");
            assert_eq!(
                read(dir.path(), &device, 256),
                Some(Quota {
                    limit_bytes: 1_073_741_824,
                    referenced_bytes: 4096,
                })
            );
            assert_eq!(read(dir.path(), &device, 257), None);
            assert_eq!(read(dir.path(), &device, 258), None);
            assert_eq!(read(dir.path(), &device, 999), None);
            assert_eq!(read(dir.path(), &OsString::from("sdz9"), 256), None);
        }
    }
}
