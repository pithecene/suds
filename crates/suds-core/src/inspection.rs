//! The lossless result of an inspection, and its conversion to the
//! `suds.inspection/v1` report.
//!
//! The scan records each path and name in its native form, because an
//! operating-system path need not be valid UTF-8. Only the conversion to a
//! report replaces invalid sequences, so a displayed name never serves as the
//! identity of a file.

use std::env;
use std::ffi::OsString;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::model::{
    Child, EntryKind, ErrorKind, Measure, Platform, Report, ReportStatus, RootResult, RootStatus,
    SCHEMA, TraversalError,
};
use crate::platform::RootSpec;
use crate::scan::entry_kind;
use crate::timestamp;

/// One root's scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootScan {
    pub spec: RootSpec,
    pub status: RootStatus,
    /// Whether a cancellation request ended the scan early. A cancelled scan
    /// is `partial`.
    pub cancelled: bool,
    pub elapsed: Duration,
    pub measure: Measure,
    /// Every direct child, in ranking order. A cancelled scan lists only the
    /// children whose measurement finished.
    pub children: Vec<ChildScan>,
    pub skipped_mount_count: u64,
    /// The first skipped mount points, in traversal order.
    pub skipped_mounts: Vec<PathBuf>,
    pub error_count: u64,
    /// The first traversal errors, in traversal order.
    pub errors: Vec<ScanError>,
}

/// A direct child of a root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildScan {
    pub name: OsString,
    pub kind: EntryKind,
    /// The child's own metadata at the time of the scan. A move plan compares
    /// it with the child's metadata at planning, so the plan acts only on the
    /// entry that the inspection showed. The report omits it.
    pub revision: EntryRevision,
    pub measure: Measure,
}

/// The metadata that distinguishes one version of an entry from another.
/// The device and the inode name the entry whatever its path. The size and
/// the modification time change with its content, and a directory's
/// modification time changes when an entry enters or leaves the directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryRevision {
    pub device: u64,
    pub inode: u64,
    pub kind: EntryKind,
    pub logical_size: u64,
    pub modified_seconds: i64,
    pub modified_nanoseconds: i64,
}

impl EntryRevision {
    /// The revision of the entry that `meta` describes. The caller obtains
    /// `meta` without following a symbolic link.
    pub fn from_metadata(meta: &Metadata) -> Self {
        EntryRevision {
            device: meta.dev(),
            inode: meta.ino(),
            kind: entry_kind(meta.file_type()),
            logical_size: meta.len(),
            modified_seconds: meta.mtime(),
            modified_nanoseconds: meta.mtime_nsec(),
        }
    }

    /// Whether both revisions describe the same entry, whether or not its
    /// content changed.
    pub fn is_same_entry(&self, other: &EntryRevision) -> bool {
        self.device == other.device && self.inode == other.inode && self.kind == other.kind
    }
}

/// A path that the scan could not measure, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanError {
    pub path: PathBuf,
    pub kind: ErrorKind,
    pub message: String,
}

impl RootScan {
    /// Whether the scan measured the root, completely or in part.
    pub fn is_measured(&self) -> bool {
        matches!(self.status, RootStatus::Complete | RootStatus::Partial)
    }

    /// The root's entry in a `suds.inspection/v1` report.
    pub fn to_result(&self) -> RootResult {
        RootResult {
            id: self.spec.id.clone(),
            kind: self.spec.kind,
            path: self.spec.path.to_string_lossy().into_owned(),
            status: self.status,
            elapsed_ms: u64::try_from(self.elapsed.as_millis()).unwrap_or(u64::MAX),
            measure: self.measure.clone(),
            children: self
                .children
                .iter()
                .map(|child| Child {
                    name: child.name.to_string_lossy().into_owned(),
                    kind: child.kind,
                    measure: child.measure.clone(),
                })
                .collect(),
            skipped_mount_count: self.skipped_mount_count,
            skipped_mounts: self
                .skipped_mounts
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            error_count: self.error_count,
            errors: self
                .errors
                .iter()
                .map(|error| TraversalError {
                    path: error.path.to_string_lossy().into_owned(),
                    kind: error.kind,
                    message: error.message.clone(),
                })
                .collect(),
        }
    }
}

/// Builds a `suds.inspection/v1` report from the scans of one run.
///
/// The `observed_at` argument holds the start time of the run, in seconds
/// since the Unix epoch.
pub fn build_report(scans: &[RootScan], observed_at: i64) -> Report {
    let status = if scans
        .iter()
        .all(|scan| matches!(scan.status, RootStatus::Complete | RootStatus::Absent))
    {
        ReportStatus::Complete
    } else {
        ReportStatus::Partial
    };
    Report {
        schema: SCHEMA.to_owned(),
        platform: Platform {
            os: env::consts::OS.to_owned(),
            arch: env::consts::ARCH.to_owned(),
        },
        observed_at: timestamp::format_utc(observed_at),
        status,
        roots: scans.iter().map(RootScan::to_result).collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    use tempfile::TempDir;

    use crate::model::{RootKind, RootStatus};
    use crate::platform::RootSpec;
    use crate::scan::scan_root;

    fn spec(path: &Path) -> RootSpec {
        RootSpec {
            id: "test".to_owned(),
            kind: RootKind::Unclassified,
            path: path.to_path_buf(),
            anchor: None,
        }
    }

    #[test]
    fn a_cancelled_scan_is_partial_and_says_so() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        fs::write(dir.path().join("a/file"), b"data").unwrap();

        let scan = scan_root(&spec(dir.path()), &AtomicBool::new(true));
        assert!(scan.cancelled);
        assert_eq!(scan.status, RootStatus::Partial);
        assert!(scan.children.is_empty());
        assert_eq!(scan.to_result().status, RootStatus::Partial);

        let scan = scan_root(&spec(dir.path()), &AtomicBool::new(false));
        assert!(!scan.cancelled);
        assert_eq!(scan.status, RootStatus::Complete);
        assert_eq!(scan.measure.files, 1);
    }

    /// macOS rejects a file name that is not valid UTF-8, so this test runs
    /// on Linux alone.
    #[cfg(target_os = "linux")]
    #[test]
    fn keeps_a_non_utf8_name_intact_and_replaces_it_only_in_the_report() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = TempDir::new().unwrap();
        let name = OsStr::from_bytes(b"caf\xe9");
        fs::write(dir.path().join(name), b"data").unwrap();

        let scan = scan_root(&spec(dir.path()), &AtomicBool::new(false));
        assert_eq!(scan.children[0].name, name);
        assert!(dir.path().join(&scan.children[0].name).exists());
        assert_eq!(scan.to_result().children[0].name, "caf\u{fffd}");
    }
}
