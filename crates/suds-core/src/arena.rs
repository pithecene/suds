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

use crate::{pressure, quota};

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

/// The filesystem that holds `path`, by the name that the kernel reports.
/// A volume of another type, such as a network volume, takes no APFS quota.
#[cfg(target_os = "macos")]
pub fn filesystem_of(path: &Path) -> Filesystem {
    let Ok(stat) = rustix::fs::statfs(path) else {
        return Filesystem::Other;
    };
    let name: Vec<u8> = stat
        .f_fstypename
        .iter()
        .take_while(|unit| **unit != 0)
        .map(|unit| unit.cast_unsigned())
        .collect();
    if name == b"apfs" {
        Filesystem::Apfs
    } else {
        Filesystem::Other
    }
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
    /// The limit that a quota group of btrfs places on the root's
    /// subvolume, or none.
    pub quota_limit_bytes: Option<u64>,
    pub ceiling_bytes: u64,
    pub ceiling_source: CeilingSource,
    /// Whether a filesystem enforces the ceiling on the root and on every
    /// other location that the audit received.
    pub enforced: bool,
    /// Each other root or store that lies outside the root's filesystem,
    /// so no limit of that filesystem bounds it.
    pub uncovered: Vec<String>,
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
/// filesystem of its own whose `bound` is no more than the ceiling. The
/// check reads what any user can read, so a project quota of XFS stays
/// invisible, and such a root reads as not enforced.
pub fn is_enforced(root_device: u64, system_device: u64, bound: u64, ceiling: u64) -> bool {
    root_device != system_device && bound <= ceiling
}

/// The most that a root's filesystem lets it hold: the filesystem's size,
/// or the limit of the root's quota group where that is lower.
fn bound_of(capacity: u64, limit: Option<u64>) -> u64 {
    limit.map_or(capacity, |limit| limit.min(capacity))
}

/// Audits the arena at `root`. The settings name the ceiling, and the
/// default follows the disk's size. The `others` hold each other root and
/// each store whose bytes count toward the pressure, and a limit holds only
/// when it covers every one of them.
pub fn audit(
    root: &Path,
    others: &[PathBuf],
    settings_ceiling: Option<u64>,
) -> Result<Arena, String> {
    let failed = |path: &Path, err: std::io::Error| {
        format!("could not read the metadata of {}: {err}", path.display())
    };
    // The audit follows no link: a root behind one would mix the link's
    // place with its target's filesystem.
    let meta = fs::symlink_metadata(root).map_err(|err| failed(root, err))?;
    let canonical = fs::canonicalize(root).map_err(|err| failed(root, err))?;
    if meta.file_type().is_symlink() || canonical != root {
        return Err(format!(
            "{} is a symbolic link or lies behind one, and Suds audits no root through a link. Name {} instead.",
            root.display(),
            canonical.display()
        ));
    }
    let root_device = meta.dev();
    let uncovered: Vec<String> = others
        .iter()
        .filter(|other| fs::metadata(other).map_or(true, |meta| meta.dev() != root_device))
        .map(|other| other.to_string_lossy().into_owned())
        .collect();
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
    let limit = quota::of(root).map(|quota| quota.limit_bytes);
    let bound = bound_of(capacity, limit);
    let bounded = is_enforced(root_device, system_device, bound, ceiling);
    let enforced = bounded && uncovered.is_empty();
    let finding = if bounded && !enforced {
        format!(
            "{} lies on a filesystem within the ceiling, and {} other locations lie outside that filesystem",
            root.display(),
            uncovered.len()
        )
    } else if let (true, Some(limit)) = (enforced, limit.filter(|limit| *limit <= capacity)) {
        format!(
            "a quota group limits the subvolume of {} to {} GiB, within the ceiling",
            root.display(),
            limit / GIB
        )
    } else if enforced {
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
    } else if let Some(limit) = limit {
        format!(
            "a quota group limits the subvolume of {} to {} GiB, more than the ceiling",
            root.display(),
            limit / GIB
        )
    } else {
        let quota = if filesystem == Filesystem::Btrfs {
            "no quota group limits it"
        } else {
            "Suds can read no quota beneath it"
        };
        format!(
            "the filesystem of {} holds {} GiB, more than the ceiling, and {quota}",
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
        quota_limit_bytes: limit,
        ceiling_bytes: ceiling,
        ceiling_source: source,
        enforced,
        uncovered,
        finding,
        provisioning: if bounded {
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
    let place = shell_quote(&arena.to_string_lossy());
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
                format!("sudo btrfs subvolume create {place}"),
                true,
                "Create a subvolume for the development tree.",
            ),
            step(
                format!("sudo btrfs quota enable {place}"),
                true,
                "Turn on quota groups for the filesystem.",
            ),
            step(
                format!("sudo btrfs qgroup limit {gib}G {place}"),
                true,
                "Limit the subvolume to the ceiling.",
            ),
            step(
                format!("sudo chown \"$USER\" {place}"),
                true,
                "Give the subvolume to the user who develops in it.",
            ),
        ],
        Filesystem::Xfs => {
            let target = shell_quote(&root.to_string_lossy());
            let project = shell_quote(&format!("project -s -p {} 4242", arena.display()));
            let limit = shell_quote(&format!("limit -p bhard={gib}g 4242"));
            vec![
                step(
                    format!("findmnt -no TARGET,OPTIONS -T {target}"),
                    false,
                    "Read the mount point of the root's filesystem, and confirm that its options hold prjquota. A remount with that option needs an administrator.",
                ),
                step(
                    format!("sudo xfs_quota -x -c {project} MOUNTPOINT"),
                    true,
                    "Make the development tree a project.",
                ),
                step(
                    format!("sudo xfs_quota -x -c {limit} MOUNTPOINT"),
                    true,
                    "Limit the project to the ceiling.",
                ),
            ]
        }
        Filesystem::Ext4 | Filesystem::Other => vec![step(
            format!("a partition or a logical volume of {gib} GiB, mounted at {place}"),
            true,
            "This filesystem offers no limit on a directory that fits every kernel, so a filesystem of the ceiling's size holds the development tree.",
        )],
    }
}

/// Quotes `text` as one word of a shell command. A person copies each
/// command of a plan into a shell, where a space would split a path and
/// another character could run a command.
fn shell_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "/._-+=:,@".contains(character));
    if plain {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use std::fs;
    use std::path::PathBuf;

    use super::{
        CeilingSource, Filesystem, GIB, audit, bound_of, is_enforced, provisioning, shell_quote,
    };

    #[test]
    fn a_filesystem_of_its_own_within_the_ceiling_enforces_it() {
        assert!(is_enforced(2, 1, 200 * GIB, 200 * GIB));
        // The system's own filesystem enforces nothing, whatever its size.
        assert!(!is_enforced(1, 1, 200 * GIB, 500 * GIB));
        // A larger filesystem without a limit bounds nothing.
        assert!(!is_enforced(2, 1, bound_of(1900 * GIB, None), 950 * GIB));
        // A quota group's limit bounds a subvolume of a larger filesystem.
        let limited = bound_of(1900 * GIB, Some(900 * GIB));
        assert!(is_enforced(2, 1, limited, 950 * GIB));
        assert!(!is_enforced(2, 1, bound_of(1900 * GIB, Some(960 * GIB)), 950 * GIB));
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

        let xfs = provisioning(Filesystem::Xfs, root, 100 * GIB);
        // The first step reads the audited root's filesystem.
        assert_eq!(
            xfs[0].command,
            "findmnt -no TARGET,OPTIONS -T /home/a/workspace"
        );
        assert_eq!(
            xfs[2].command,
            "sudo xfs_quota -x -c 'limit -p bhard=100g 4242' MOUNTPOINT"
        );
        assert_eq!(provisioning(Filesystem::Ext4, root, 100 * GIB).len(), 1);
    }

    #[test]
    fn quotes_each_path_as_one_word_of_a_shell() {
        assert_eq!(shell_quote("/home/a/work"), "/home/a/work");
        assert_eq!(shell_quote("/home/a/work space"), "'/home/a/work space'");
        assert_eq!(shell_quote("a'b; rm -rf /"), "'a'\\''b; rm -rf /'");
        let root = Path::new("/home/a/work space");
        let btrfs = provisioning(Filesystem::Btrfs, root, 100 * GIB);
        assert_eq!(
            btrfs[0].command,
            "sudo btrfs subvolume create '/home/a/work space-arena'"
        );
        let xfs = provisioning(Filesystem::Xfs, root, 100 * GIB);
        assert_eq!(
            xfs[1].command,
            "sudo xfs_quota -x -c 'project -s -p /home/a/work space-arena 4242' MOUNTPOINT"
        );
    }

    #[test]
    fn audits_a_directory_against_a_ceiling_of_the_settings() {
        let dir = TempDir::new().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let arena = audit(&root, &[], Some(GIB)).unwrap();
        assert_eq!(arena.ceiling_bytes, GIB);
        assert_eq!(arena.ceiling_source, CeilingSource::Settings);
        assert!(arena.capacity_bytes > 0);
        // A temporary directory lies on no filesystem of one gibibyte.
        assert!(!arena.enforced);
        assert!(!arena.provisioning.is_empty());

        let by_default = audit(&root, &[], None).unwrap();
        assert_eq!(by_default.ceiling_source, CeilingSource::Default);
        assert!(by_default.ceiling_bytes >= 100 * GIB);
        assert!(audit(&root.join("missing"), &[], None).is_err());
    }

    #[test]
    fn audits_no_root_through_a_symbolic_link() {
        let dir = TempDir::new().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        let refused = audit(&root.join("link"), &[], None).unwrap_err();
        assert!(refused.contains("symbolic link"), "{refused}");
        assert!(audit(&root.join("link/.."), &[], None).is_err());
        assert!(audit(&root.join("real"), &[], None).is_ok());
    }

    #[test]
    fn a_location_outside_the_roots_filesystem_is_uncovered() {
        let dir = TempDir::new().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir(root.join("store")).unwrap();
        // A location that does not exist lies on no filesystem at all.
        let others = [root.join("store"), PathBuf::from("/no/such/store")];
        let arena = audit(&root, &others, None).unwrap();
        assert_eq!(arena.uncovered, ["/no/such/store"]);
        assert!(!arena.enforced);
    }
}
