//! The inspection report, which the program serializes as JSON field by field.
//!
//! Serde writes each struct's fields to the JSON document in declaration
//! order. Each enum variant's name appears in snake case.

use serde::{Deserialize, Serialize};

/// The schema tag of every report.
pub const SCHEMA: &str = "suds.inspection/v1";

/// One run of `suds inspect`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub schema: String,
    pub platform: Platform,
    /// The time at which the run started, in RFC 3339 form and UTC.
    pub observed_at: String,
    pub status: ReportStatus,
    pub roots: Vec<RootResult>,
}

/// The operating system and architecture of the machine that ran the
/// inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub os: String,
    pub arch: String,
}

/// Whether every present root yielded a complete measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    Complete,
    Partial,
}

/// The descriptive class of a root.
///
/// A kind describes what a root usually holds. It grants no authority to
/// remove anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    Cache,
    Logs,
    Generated,
    Inbox,
    Trash,
    /// A root that the caller named with `--root`.
    Unclassified,
}

impl RootKind {
    pub fn label(self) -> &'static str {
        match self {
            RootKind::Cache => "cache",
            RootKind::Logs => "logs",
            RootKind::Generated => "generated",
            RootKind::Inbox => "inbox",
            RootKind::Trash => "trash",
            RootKind::Unclassified => "unclassified",
        }
    }
}

/// The outcome of one root's traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootStatus {
    /// The scan measured every entry beneath the root.
    Complete,
    /// The scan measured the root but could not measure one or more entries
    /// beneath it.
    Partial,
    /// No entry exists at the root's path.
    Absent,
    /// The root exists, but the scan could not read its metadata or list its
    /// entries.
    Unreadable,
}

impl RootStatus {
    pub fn label(self) -> &'static str {
        match self {
            RootStatus::Complete => "complete",
            RootStatus::Partial => "partial",
            RootStatus::Absent => "absent",
            RootStatus::Unreadable => "unreadable",
        }
    }
}

/// The class of a single filesystem entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

/// Storage and entry counts for a root or for one of its direct children.
///
/// A child's measure covers the child and every entry beneath it. A root's
/// measure is the sum of its children's and excludes the root directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Measure {
    /// The sum of regular-file sizes, counting each hard-linked file once.
    pub logical_bytes: u64,
    /// The blocks that every entry occupies, counting each hard-linked file
    /// once. The figure is an estimate (see the README).
    pub allocated_bytes: u64,
    pub files: u64,
    pub directories: u64,
    pub symlinks: u64,
    pub others: u64,
    pub oldest_modified: Option<String>,
    pub newest_modified: Option<String>,
}

/// One root's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootResult {
    pub id: String,
    pub kind: RootKind,
    pub path: String,
    pub status: RootStatus,
    pub elapsed_ms: u64,
    #[serde(flatten)]
    pub measure: Measure,
    /// Every direct child, in ranking order. A cancelled scan lists only the
    /// children whose measurement finished.
    pub children: Vec<Child>,
    pub skipped_mount_count: u64,
    /// The first skipped mount points, in traversal order.
    pub skipped_mounts: Vec<String>,
    pub error_count: u64,
    /// The first traversal errors, in traversal order.
    pub errors: Vec<TraversalError>,
}

/// A direct child of a root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Child {
    pub name: String,
    pub kind: EntryKind,
    #[serde(flatten)]
    pub measure: Measure,
}

/// The class of a traversal error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    PermissionDenied,
    /// The entry disappeared between its listing and its measurement.
    Vanished,
    Other,
}

/// A path that the scan could not measure, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraversalError {
    pub path: String,
    pub kind: ErrorKind,
    pub message: String,
}
