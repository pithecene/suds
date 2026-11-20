//! What the Suds window shows.
//!
//! This crate turns the core's measurements into the values of one view: the
//! pressure, the classes of development storage, each checkout, and each
//! store. It names no interface toolkit, so a test reads a view without a
//! display, and the window of each system draws the same values.

mod format;
mod observe;
mod workstation;

pub use format::{bytes, day};
pub use observe::{Observation, observe};
pub use workstation::{
    BuildState, Checkout, Class, Hold, PressureCard, Sort, Store, StoreStanding, Workstation,
    arrange, workstation,
};
