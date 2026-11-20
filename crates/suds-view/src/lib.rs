//! What the Suds window shows.
//!
//! This crate turns the core's measurements into the values of one view: the
//! pressure, the classes of development storage, each project with its
//! checkouts, the storage map, and each store. It names no interface toolkit, so a test reads a view without a
//! display, and the window of each system draws the same values.

mod format;
mod map;
mod observe;
mod project;
mod remote;
mod workstation;

pub use format::{bytes, day};
pub use map::{Frame, LABEL, Map, Rect, Tile, map, squarify};
pub use observe::{Observation, observe};
pub use project::{Project, projects};
pub use workstation::{
    BuildState, Checkout, Class, Hold, PressureCard, Sort, Store, StoreStanding, Workstation,
    arrange, workstation,
};
