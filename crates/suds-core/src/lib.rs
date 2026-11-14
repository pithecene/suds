//! The workstation model, the filesystem inspection, and the reversible
//! operations that the Suds command-line interface and desktop application
//! share.
//!
//! The command line and the desktop application in this workspace call
//! this crate. Its programming interface carries no stability promise
//! beyond the workspace.

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("Suds supports macOS and Linux only.");

pub mod inspection;
pub mod model;
pub mod organize;
pub mod platform;
pub mod roots;
pub mod scan;
pub mod timestamp;
