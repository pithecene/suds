//! Linux has no default profile, so the operator names the root directories
//! for each Linux run.

use super::{ProfileError, RootSpec};

pub fn default_roots() -> Result<Vec<RootSpec>, ProfileError> {
    Err(ProfileError::Unsupported { os: "linux" })
}
