//! What the Suds window shows.
//!
//! This crate turns the core's measurements into the values of one view: the
//! pressure, the classes of development storage, each project with its
//! checkouts, the storage map, and each store. A reclaim plan takes the same
//! form for a person to read, and the core applies the approved plan. The
//! crate names no interface toolkit, so a test reads a view without a
//! display, and the window of each system draws the same values.

mod format;
mod map;
mod observe;
mod project;
mod reclaim;
mod remote;
mod workstation;

pub use format::{bytes, day};
pub use map::{Frame, LABEL, Map, Rect, Tile, map, squarify};
pub use observe::{Observation, observe};
pub use project::{Project, projects};
pub use reclaim::{Entry, IDLE_SECONDS, KeptGroup, Proposal, apply, propose};
pub use workstation::{
    BuildState, Checkout, Class, Hold, PressureCard, Sort, Store, StoreStanding, Workstation,
    arrange, workstation,
};
