//! The roots that `suds inspect` examines when the caller names none.
//!
//! Each operating system's profile lives in its own module. The scanner reads
//! only `RootSpec` values, so a new profile changes no scanning code.

use std::fmt;
use std::path::PathBuf;

use crate::model::RootKind;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub use linux::default_roots;
#[cfg(target_os = "macos")]
pub use macos::default_roots;

/// A root to inspect, with its stable identifier and descriptive kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootSpec {
    pub id: String,
    pub kind: RootKind,
    /// An absolute, lexically normalized path.
    pub path: PathBuf,
    /// For a default root, the home directory beneath which the scan accepts
    /// no symbolic link in the root's path. An explicit root has no anchor.
    pub anchor: Option<PathBuf>,
}

/// The reason no default profile applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileError {
    /// The operating system has no default profile.
    #[cfg(target_os = "linux")]
    Unsupported { os: &'static str },
    /// The process has no home directory from which to resolve the profile.
    #[cfg(target_os = "macos")]
    NoHome,
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(target_os = "linux")]
            ProfileError::Unsupported { os } => write!(
                f,
                "no default inspection profile exists for {os}. Name each root with --root PATH."
            ),
            #[cfg(target_os = "macos")]
            ProfileError::NoHome => write!(
                f,
                "no home directory exists for this process. Name each root with --root PATH."
            ),
        }
    }
}
