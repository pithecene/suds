//! The arena: the development storage and its ceiling.
//!
//! Every softer bound can fail, so the ceiling should rest on a limit that a
//! filesystem enforces. Creating that limit needs a privileged command, and
//! Suds requests no privilege. The audit therefore reports whether a
//! filesystem enforces the ceiling, and the provisioning plan names each
//! command for a person to run.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::pressure;

/// The schema tag of every arena report.
pub const ARENA_SCHEMA: &str = "suds.arena/v1";

const GIB: u64 = 1024 * 1024 * 1024;

/// The filesystem that holds the arena, by its type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Filesystem {
    Apfs,
    Btrfs,
    Xfs,
    Ext4,
    Other,
}

/// The filesystem that holds `path`. Every user volume of a current macOS
/// is an APFS volume.
#[cfg(target_os = "macos")]
pub fn filesystem_of(_path: &Path) -> Filesystem {
    Filesystem::Apfs
}

/// The filesystem that holds `path`, by the type that the kernel reports.
#[cfg(target_os = "linux")]
pub fn filesystem_of(path: &Path) -> Filesystem {
    const BTRFS: i64 = 0x9123_683E;
    const XFS: i64 = 0x5846_5342;
    const EXT4: i64 = 0xEF53;
    match rustix::fs::statfs(path).map(|stat| stat.f_type) {
        Ok(BTRFS) => Filesystem::Btrfs,
        Ok(XFS) => Filesystem::Xfs,
        Ok(EXT4) => Filesystem::Ext4,
        _ => Filesystem::Other,
    }
}

/// Where a ceiling comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CeilingSource {
    /// The greater of half of the disk and 100 GiB.
    Default,
    /// The owner's settings.
    Settings,
}

/// The audit of one arena.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Arena {
    pub schema: String,
    pub root: String,
    pub filesystem: Filesystem,
    /// The size of the filesystem that holds the root, in bytes.
    pub capacity_bytes: u64,
    pub available_bytes: u64,
    pub ceiling_bytes: u64,
    pub ceiling_source: CeilingSource,
    /// Whether a filesystem enforces the ceiling.
    pub enforced: bool,
    /// The fact that decides `enforced`.
    pub finding: String,
    /// Each command that gives the ceiling a filesystem limit, or none when
    /// one enforces it already.
    pub provisioning: Vec<Step>,
}

/// One command of a provisioning plan. Suds runs none of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Step {
    pub command: String,
    /// Whether the command needs an administrator.
    pub privileged: bool,
    pub purpose: String,
}

/// Whether a filesystem enforces `ceiling` on a root: the root lies on a
/// filesystem of its own that is no larger than the ceiling. The check
/// reads what any user can read, so a quota group of btrfs and a project
/// quota of XFS stay invisible, and such a root reads as not enforced.
pub fn is_enforced(root_device: u64, system_device: u64, capacity: u64, ceiling: u64) -> bool {
    root_device != system_device && capacity <= ceiling
}

/// Audits the arena at `root`. The settings name the ceiling, and the
/// default follows the disk's size.
pub fn audit(root: &Path, settings_ceiling: Option<u64>) -> Result<Arena, String> {
    let failed = |path: &Path, err: std::io::Error| {
        format!("could not read the metadata of {}: {err}", path.display())
    };
    let root_device = fs::metadata(root).map_err(|err| failed(root, err))?.dev();
    let system = Path::new("/");
    let system_device = fs::metadata(system)
        .map_err(|err| failed(system, err))?
        .dev();
    let (capacity, available) = pressure::filesystem(root)
        .ok_or_else(|| format!("could not read the filesystem of {}", root.display()))?;
    let (ceiling, source) = match settings_ceiling {
        Some(ceiling) => (ceiling, CeilingSource::Settings),
        None => {
            // The default follows the size of the system's disk, because a
            // volume under a quota reports its quota as its own size.
            let disk = pressure::filesystem(system).map_or(capacity, |(size, _)| size);
            (
                pressure::default_ceiling(disk.max(capacity)),
                CeilingSource::Default,
            )
        }
    };
    let filesystem = filesystem_of(root);
    let enforced = is_enforced(root_device, system_device, capacity, ceiling);
    let finding = if enforced {
        format!(
            "{} lies on a filesystem of its own of {} GiB, within the ceiling",
            root.display(),
            capacity / GIB
        )
    } else if root_device == system_device {
        format!(
            "{} shares the system's filesystem, so development storage can fill the system's volume",
            root.display()
        )
    } else {
        format!(
            "the filesystem of {} holds {} GiB, more than the ceiling, and Suds can read no quota beneath it",
            root.display(),
            capacity / GIB
        )
    };
    Ok(Arena {
        schema: ARENA_SCHEMA.to_owned(),
        root: root.to_string_lossy().into_owned(),
        filesystem,
        capacity_bytes: capacity,
        available_bytes: available,
        ceiling_bytes: ceiling,
        ceiling_source: source,
        enforced,
        finding,
        provisioning: if enforced {
            Vec::new()
        } else {
            provisioning(filesystem, root, ceiling)
        },
    })
}

/// The commands that give `ceiling` a filesystem limit at `root`. The plan
/// creates a new, empty location under the limit, and it moves nothing: the
/// owner moves the development tree there once the limit holds.
pub fn provisioning(filesystem: Filesystem, root: &Path, ceiling: u64) -> Vec<Step> {
    let gib = (ceiling / GIB).max(1);
    let step = |command: String, privileged: bool, purpose: &str| Step {
        command,
        privileged,
        purpose: purpose.to_owned(),
    };
    let arena: PathBuf = root.with_file_name(format!(
        "{}-arena",
        root.file_name()
            .map_or_else(|| "development".into(), |name| name.to_string_lossy())
    ));
    let arena = arena.display();
    match filesystem {
        Filesystem::Apfs => vec![
            step(
                "diskutil apfs list".to_owned(),
                false,
                "Find the container that holds the system's volume, such as disk3.",
            ),
            step(
                format!("sudo diskutil apfs addVolume CONTAINER APFS SudsArena -quota {gib}g"),
                true,
                "Add a volume whose quota is the ceiling. The volume shares the container's free space and can never exceed its quota.",
            ),
            step(
                "diskutil info /Volumes/SudsArena".to_owned(),
                false,
                "Confirm the volume's quota before the development tree moves there.",
            ),
        ],
        Filesystem::Btrfs => vec![
            step(
                format!("sudo btrfs subvolume create {arena}"),
                true,
                "Create a subvolume for the development tree.",
            ),
            step(
                format!("sudo btrfs quota enable {arena}"),
                true,
                "Turn on quota groups for the filesystem.",
            ),
            step(
                format!("sudo btrfs qgroup limit {gib}G {arena}"),
                true,
                "Limit the subvolume to the ceiling.",
            ),
            step(
                format!("sudo chown \"$USER\" {arena}"),
                true,
                "Give the subvolume to the user who develops in it.",
            ),
        ],
        Filesystem::Xfs => vec![
            step(
                "findmnt -no OPTIONS -T .".to_owned(),
                false,
                "Confirm that the filesystem's mount options hold prjquota. A remount with that option needs an administrator.",
            ),
            step(
                format!("sudo xfs_quota -x -c 'project -s -p {arena} 4242' MOUNTPOINT"),
                true,
                "Make the development tree a project.",
            ),
            step(
                format!("sudo xfs_quota -x -c 'limit -p bhard={gib}g 4242' MOUNTPOINT"),
                true,
                "Limit the project to the ceiling.",
            ),
        ],
        Filesystem::Ext4 | Filesystem::Other => vec![step(
            format!("a partition or a logical volume of {gib} GiB, mounted at {arena}"),
            true,
            "This filesystem offers no limit on a directory that fits every kernel, so a filesystem of the ceiling's size holds the development tree.",
        )],
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::{CeilingSource, Filesystem, GIB, audit, is_enforced, provisioning};

    #[test]
    fn a_filesystem_of_its_own_within_the_ceiling_enforces_it() {
        assert!(is_enforced(2, 1, 200 * GIB, 200 * GIB));
        // The system's own filesystem enforces nothing, whatever its size.
        assert!(!is_enforced(1, 1, 200 * GIB, 500 * GIB));
        // A larger filesystem may hold a quota that Suds cannot read.
        assert!(!is_enforced(2, 1, 1900 * GIB, 950 * GIB));
    }

    #[test]
    fn names_each_command_for_each_filesystem_and_runs_none() {
        let root = Path::new("/home/a/workspace");
        let apfs = provisioning(Filesystem::Apfs, root, 500 * GIB);
        assert!(apfs[1].command.ends_with("SudsArena -quota 500g"));
        assert!(apfs[1].privileged);

        let btrfs = provisioning(Filesystem::Btrfs, root, 950 * GIB);
        let commands: Vec<&str> = btrfs.iter().map(|step| step.command.as_str()).collect();
        assert_eq!(
            commands[..3],
            [
                "sudo btrfs subvolume create /home/a/workspace-arena",
                "sudo btrfs quota enable /home/a/workspace-arena",
                "sudo btrfs qgroup limit 950G /home/a/workspace-arena",
            ]
        );
        assert!(btrfs.iter().all(|step| step.privileged));

        assert!(
            provisioning(Filesystem::Xfs, root, 100 * GIB)[2]
                .command
                .contains("bhard=100g")
        );
        assert_eq!(provisioning(Filesystem::Ext4, root, 100 * GIB).len(), 1);
    }

    #[test]
    fn audits_a_directory_against_a_ceiling_of_the_settings() {
        let dir = TempDir::new().unwrap();
        let arena = audit(dir.path(), Some(GIB)).unwrap();
        assert_eq!(arena.ceiling_bytes, GIB);
        assert_eq!(arena.ceiling_source, CeilingSource::Settings);
        assert!(arena.capacity_bytes > 0);
        // A temporary directory lies on no filesystem of one gibibyte.
        assert!(!arena.enforced);
        assert!(!arena.provisioning.is_empty());

        let by_default = audit(dir.path(), None).unwrap();
        assert_eq!(by_default.ceiling_source, CeilingSource::Default);
        assert!(by_default.ceiling_bytes >= 100 * GIB);
        assert!(audit(&dir.path().join("missing"), None).is_err());
    }
}
