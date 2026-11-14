//! Reversible organization of a directory's direct children.
//!
//! Each step of an operation is explicit. The planner reads metadata and builds a plan that names every source and
//! destination. A person reviews the plan. The executor then verifies each
//! move's preconditions, renames the entry within one filesystem, and
//! records each step in the journal, from which an undo restores the
//! entries. Nothing in this module deletes, copies, or overwrites an entry.

pub mod execute;
pub mod journal;
pub mod native;
pub mod plan;
