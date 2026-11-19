//! Removal of derived development state.
//!
//! The planner names each directory of build state and each linked checkout
//! that a development inspection found, with the evidence for keeping or
//! removing it. The executor verifies that evidence again immediately before
//! each removal and records each step in the journal. A build regenerates
//! build state, and a remote holds a removed checkout's work, so a removal
//! here is permanent.

pub mod execute;
pub mod git;
pub mod liveness;
pub mod plan;
