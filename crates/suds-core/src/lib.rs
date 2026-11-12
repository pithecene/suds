//! The workstation model and the filesystem inspection that the Suds
//! command-line interface and desktop application share.
//!
//! This crate serves the interfaces in this workspace and publishes no
//! stable interface beyond it.

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("Suds supports macOS and Linux only.");

pub mod inspection;
pub mod model;
pub mod platform;
pub mod roots;
pub mod scan;
pub mod timestamp;
