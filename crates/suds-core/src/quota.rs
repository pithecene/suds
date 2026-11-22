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
/// group limits it. An error states why Suds could not read the limit of a
/// btrfs filesystem, so a failed reading never passes for no limit.
#[cfg(target_os = "linux")]
pub fn of(path: &Path) -> Result<Option<Quota>, String> {
    let table = Path::new("/proc/self/mountinfo");
    let mounts = std::fs::read_to_string(table)
        .map_err(|err| format!("could not read {}: {err}", table.display()))?;
    let Some(device) = linux::device(&mounts, path) else {
        return Ok(None);
    };
    let subvolume = linux::subvolume(path)?;
    linux::read(Path::new("/sys/fs/btrfs"), &device, subvolume)
}

/// No other system holds a quota group.
#[cfg(not(target_os = "linux"))]
pub fn of(_path: &Path) -> Result<Option<Quota>, String> {
    Ok(None)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsString;
    use std::fs;
    use std::io::ErrorKind;
    use std::path::{Path, PathBuf};

    use rustix::fs::{AtFlags, CWD, StatxFlags, statx};

    use super::Quota;

    /// The bit of `statx` that asks for a subvolume, which Linux 6.10 added.
    const STATX_SUBVOL: u32 = 0x8000;

    /// The flag of a quota group that holds a limit on referenced bytes.
    const LIMIT_REFERENCED: u64 = 1;

    /// The subvolume that holds `path`, by its identifier.
    pub fn subvolume(path: &Path) -> Result<u64, String> {
        let stat = statx(
            CWD,
            path,
            AtFlags::SYMLINK_NOFOLLOW,
            StatxFlags::from_bits_retain(STATX_SUBVOL),
        )
        .map_err(|err| format!("could not read the metadata of {}: {err}", path.display()))?;
        if stat.stx_mask & STATX_SUBVOL == 0 {
            return Err(format!(
                "this kernel names no subvolume of {}, which Linux 6.10 or later does",
                path.display()
            ));
        }
        Ok(stat.stx_subvol)
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
    /// `device`, beneath the kernel's directory `published`. A filesystem
    /// without quota groups publishes no group, which is no limit.
    pub fn read(
        published: &Path,
        device: &OsString,
        subvolume: u64,
    ) -> Result<Option<Quota>, String> {
        let failed =
            |path: &Path, err: std::io::Error| format!("could not read {}: {err}", path.display());
        let mut filesystem = None;
        for entry in fs::read_dir(published).map_err(|err| failed(published, err))? {
            let entry = entry.map_err(|err| failed(published, err))?;
            let listed = entry.path().join("devices").join(device);
            match fs::symlink_metadata(&listed) {
                Ok(_) => filesystem = Some(entry.path()),
                Err(err) if absent(&err) => {}
                Err(err) => return Err(failed(&listed, err)),
            }
        }
        let Some(filesystem) = filesystem else {
            return Err(format!(
                "no filesystem beneath {} lists the device {}",
                published.display(),
                device.display()
            ));
        };
        let group = filesystem.join("qgroups").join(format!("0_{subvolume}"));
        let number = |name: &str| -> Result<Option<u64>, String> {
            let file = group.join(name);
            match fs::read_to_string(&file) {
                Ok(text) => text
                    .trim()
                    .parse()
                    .map(Some)
                    .map_err(|err| format!("{} holds no number: {err}", file.display())),
                Err(err) if absent(&err) => Ok(None),
                Err(err) => Err(failed(&file, err)),
            }
        };
        let Some(flags) = number("limit_flags")? else {
            return Ok(None);
        };
        if flags & LIMIT_REFERENCED == 0 {
            return Ok(None);
        }
        let stated = |name: &str| {
            number(name)?.ok_or_else(|| format!("{} is absent", group.join(name).display()))
        };
        Ok(Some(Quota {
            limit_bytes: stated("max_referenced")?,
            referenced_bytes: stated("referenced")?,
        }))
    }

    /// Whether an error states that a path holds no entry.
    fn absent(err: &std::io::Error) -> bool {
        matches!(err.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
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
                Ok(Some(Quota {
                    limit_bytes: 1_073_741_824,
                    referenced_bytes: 4096,
                }))
            );
            assert_eq!(read(dir.path(), &device, 257), Ok(None));
            assert_eq!(read(dir.path(), &device, 258), Ok(None));
            // A subvolume without a group has no limit.
            assert_eq!(read(dir.path(), &device, 999), Ok(None));
        }

        #[test]
        fn a_reading_that_fails_is_an_error_and_no_absent_limit() {
            let dir = TempDir::new().unwrap();
            let filesystem = dir.path().join("0723c641");
            fs::create_dir_all(filesystem.join("devices/sdb1")).unwrap();
            let group = filesystem.join("qgroups/0_256");
            fs::create_dir_all(&group).unwrap();
            let device = OsString::from("sdb1");

            fs::write(group.join("limit_flags"), "many\n").unwrap();
            let damaged = read(dir.path(), &device, 256).unwrap_err();
            assert!(damaged.contains("holds no number"), "{damaged}");

            // A group that states a limit and holds no figure proves none.
            fs::write(group.join("limit_flags"), "1\n").unwrap();
            let partial = read(dir.path(), &device, 256).unwrap_err();
            assert!(partial.contains("max_referenced is absent"), "{partial}");

            // A directory stands in a figure's place, so the reading fails.
            fs::write(group.join("max_referenced"), "1\n").unwrap();
            fs::create_dir(group.join("referenced")).unwrap();
            assert!(read(dir.path(), &device, 256).is_err());

            let unlisted = read(dir.path(), &OsString::from("sdz9"), 256).unwrap_err();
            assert!(unlisted.contains("lists the device sdz9"), "{unlisted}");
            assert!(read(&dir.path().join("missing"), &device, 256).is_err());
        }
    }
}
