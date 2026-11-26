//! The pressure of development storage against its ceiling.
//!
//! A reading states how much of the ceiling the development storage
//! occupies, as one of a few levels. A client reads the level before it
//! starts work. The level rises at once and falls only after the usage has
//! stayed lower for a transition period, so a reading near a threshold does
//! not alternate between two levels.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::development::{DevelopmentScan, StoreStatus};
use crate::timestamp;

/// The schema tag of every pressure reading.
pub const PRESSURE_SCHEMA: &str = "suds.pressure/v1";

const GIB: u64 = 1024 * 1024 * 1024;

/// How near the development storage stands to its ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// The usage lies below the soft threshold.
    Normal,
    /// The usage reached the soft threshold. A client should reclaim before
    /// it starts more work.
    Soft,
    /// The usage reached the hard threshold. A client should start no work.
    Hard,
}

/// The shares of the ceiling at which the level changes, and the time for
/// which the usage must stay lower before the level falls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    pub soft_percent: u64,
    pub hard_percent: u64,
    pub transition_seconds: i64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            soft_percent: 80,
            hard_percent: 95,
            transition_seconds: 300,
        }
    }
}

/// One reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pressure {
    pub schema: String,
    /// The time of the measurement, in RFC 3339 form and UTC.
    pub observed_at: String,
    pub level: Level,
    /// The time at which the usage first lay below the current level's
    /// threshold, while the level waits out its transition period.
    pub lower_since: Option<String>,
    pub usage_bytes: u64,
    pub ceiling_bytes: u64,
    /// Whether a filesystem limit enforces the ceiling. Without one, Suds
    /// holds the ceiling by measurement alone.
    pub enforced: bool,
    /// The free bytes of the filesystem that holds the first root.
    pub filesystem_available_bytes: Option<u64>,
    /// Whether the inspection read every entry. Without that, the usage is
    /// a lower bound, and the level never falls on such a reading.
    pub complete: bool,
}

/// The default ceiling for a disk of `capacity` bytes: the greater of half
/// of the disk and 100 GiB.
pub fn default_ceiling(capacity: u64) -> u64 {
    (capacity / 2).max(100 * GIB)
}

/// The capacity and the free bytes of the filesystem that holds `path`.
pub fn filesystem(path: &Path) -> Option<(u64, u64)> {
    let stat = rustix::fs::statvfs(path).ok()?;
    Some((
        stat.f_blocks.saturating_mul(stat.f_frsize),
        stat.f_bavail.saturating_mul(stat.f_frsize),
    ))
}

/// The allocated bytes of everything that an inspection classified or
/// measured: each workspace, each checkout with its build state, and each
/// store. The container engine keeps its own storage, which this figure
/// leaves out.
pub fn usage(scan: &DevelopmentScan) -> u64 {
    let workspaces = scan
        .workspaces
        .iter()
        .map(|workspace| workspace.unclassified.allocated_bytes);
    let checkouts = scan.checkouts.iter().map(|checkout| {
        checkout.own.allocated_bytes
            + checkout
                .build_state
                .iter()
                .map(|build| build.measure.allocated_bytes)
                .sum::<u64>()
    });
    let stores = scan
        .stores
        .iter()
        .filter(|store| !matches!(store.status, StoreStatus::Absent | StoreStatus::Unreadable))
        .map(|store| store.measure.allocated_bytes);
    workspaces.chain(checkouts).chain(stores).sum()
}

/// What a reading measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measurement {
    pub usage_bytes: u64,
    pub ceiling_bytes: u64,
    pub enforced: bool,
    pub filesystem_available_bytes: Option<u64>,
    /// Whether the inspection read every entry. A partial inspection
    /// measures a lower bound, which proves no fall of the usage.
    pub complete: bool,
    /// The time of the measurement, in seconds since the Unix epoch.
    pub now: i64,
}

/// The reading that follows `previous`. The level rises as soon as the
/// usage reaches a higher threshold. It falls only once the usage has stayed
/// below the current level's threshold for the transition period.
pub fn next(
    previous: Option<&Pressure>,
    measurement: Measurement,
    thresholds: Thresholds,
) -> Pressure {
    let share = |percent: u64| measurement.ceiling_bytes / 100 * percent;
    let raw_measured = if measurement.usage_bytes >= share(thresholds.hard_percent) {
        Level::Hard
    } else if measurement.usage_bytes >= share(thresholds.soft_percent) {
        Level::Soft
    } else {
        Level::Normal
    };
    let now = timestamp::format_utc(measurement.now);
    let held = previous.map_or(Level::Normal, |previous| previous.level);
    let (level, lower_since) = if raw_measured >= held {
        (raw_measured, None)
    } else if !measurement.complete {
        // A lower bound below the held level proves neither recovery nor a rise,
        // so keep the held level and any active recovery window intact.
        (
            held,
            previous.and_then(|previous| previous.lower_since.clone()),
        )
    } else {
        let since = previous
            .and_then(|previous| previous.lower_since.clone())
            .unwrap_or_else(|| now.clone());
        let waited_until = timestamp::format_utc(
            measurement
                .now
                .saturating_sub(thresholds.transition_seconds),
        );
        // Timestamps in this form sort in the order of their times.
        if since <= waited_until {
            (raw_measured, None)
        } else {
            (held, Some(since))
        }
    };
    Pressure {
        schema: PRESSURE_SCHEMA.to_owned(),
        observed_at: now,
        level,
        lower_since,
        usage_bytes: measurement.usage_bytes,
        ceiling_bytes: measurement.ceiling_bytes,
        enforced: measurement.enforced,
        filesystem_available_bytes: measurement.filesystem_available_bytes,
        complete: measurement.complete,
    }
}

#[cfg(test)]
mod tests {
    use super::{GIB, Level, Measurement, Thresholds, default_ceiling, next};

    fn measure(usage_gib: u64, now: i64) -> Measurement {
        Measurement {
            usage_bytes: usage_gib * GIB,
            ceiling_bytes: 100 * GIB,
            enforced: false,
            filesystem_available_bytes: None,
            complete: true,
            now,
        }
    }

    #[test]
    fn the_default_ceiling_is_half_of_the_disk_or_a_hundred_gibibytes() {
        assert_eq!(default_ceiling(1000 * GIB), 500 * GIB);
        assert_eq!(default_ceiling(2000 * GIB), 1000 * GIB);
        assert_eq!(default_ceiling(120 * GIB), 100 * GIB);
    }

    #[test]
    fn the_level_rises_at_once_and_falls_after_the_transition_period() {
        let thresholds = Thresholds::default();
        let normal = next(None, measure(50, 0), thresholds);
        assert_eq!(normal.level, Level::Normal);
        let soft = next(Some(&normal), measure(85, 10), thresholds);
        assert_eq!(soft.level, Level::Soft);
        let hard = next(Some(&soft), measure(96, 20), thresholds);
        assert_eq!(hard.level, Level::Hard);

        // The usage drops, and the level holds through the period.
        let held = next(Some(&hard), measure(70, 30), thresholds);
        assert_eq!(held.level, Level::Hard);
        assert!(held.lower_since.is_some());
        let still = next(Some(&held), measure(70, 200), thresholds);
        assert_eq!(still.level, Level::Hard);
        assert_eq!(still.lower_since, held.lower_since);
        let fell = next(Some(&still), measure(70, 330), thresholds);
        assert_eq!(fell.level, Level::Normal);
        assert_eq!(fell.lower_since, None);
    }

    #[test]
    fn a_rise_during_the_period_starts_the_period_again() {
        let thresholds = Thresholds::default();
        let soft = next(None, measure(85, 0), thresholds);
        let waiting = next(Some(&soft), measure(70, 100), thresholds);
        assert_eq!(waiting.level, Level::Soft);
        let back = next(Some(&waiting), measure(85, 200), thresholds);
        assert_eq!(back.lower_since, None);
        let waiting = next(Some(&back), measure(70, 350), thresholds);
        assert_eq!(waiting.level, Level::Soft);
        assert_eq!(
            next(Some(&waiting), measure(70, 600), thresholds).level,
            Level::Soft
        );
        assert_eq!(
            next(Some(&waiting), measure(70, 660), thresholds).level,
            Level::Normal
        );
    }

    #[test]
    fn a_partial_measurement_never_lowers_the_level() {
        let thresholds = Thresholds::default();
        let hard = next(None, measure(96, 0), thresholds);
        let partial = Measurement {
            complete: false,
            ..measure(10, 1000)
        };
        let reading = next(Some(&hard), partial, thresholds);
        assert_eq!(reading.level, Level::Hard);
        assert!(!reading.complete);
        // A partial measurement still raises the level.
        let raised = Measurement {
            complete: false,
            ..measure(96, 10)
        };
        assert_eq!(next(None, raised, thresholds).level, Level::Hard);
    }
}
