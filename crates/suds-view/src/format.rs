//! The text of a size and of a time, as every view writes them.

use jiff::tz::{Offset, TimeZone};
use jiff::{Timestamp, Zoned};

/// A count of bytes in binary units, with one decimal above a kibibyte.
pub fn bytes(count: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = count as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{count} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The time zone of each time that a view shows.
#[derive(Debug, Clone)]
pub struct Zone(TimeZone);

impl Zone {
    /// The zone of the system, or UTC on a system that states none.
    pub fn system() -> Self {
        Zone(TimeZone::system())
    }

    pub fn utc() -> Self {
        Zone(TimeZone::UTC)
    }

    /// A zone at a fixed count of hours east of UTC, or UTC for a count beyond a day.
    pub fn fixed(hours: i8) -> Self {
        let offset = Offset::from_hours(hours).unwrap_or(Offset::UTC);
        Zone(TimeZone::fixed(offset))
    }

    fn local(&self, time: &str) -> Option<Zoned> {
        let instant: Timestamp = time.parse().ok()?;
        Some(instant.to_zoned(self.0.clone()))
    }

    /// The date of an RFC 3339 time in this zone, or nothing for a time of another form.
    pub fn day(&self, time: &str) -> String {
        let local = self.local(time);
        local.map_or_else(String::new, |local| local.strftime("%Y-%m-%d").to_string())
    }

    /// The date in this zone of the instant `seconds` after the Unix epoch, or
    /// nothing for an instant beyond the calendar.
    pub fn day_at(&self, seconds: i64) -> String {
        let instant = Timestamp::from_second(seconds).ok();
        let local = instant.map(|instant| instant.to_zoned(self.0.clone()));
        local.map_or_else(String::new, |local| local.strftime("%Y-%m-%d").to_string())
    }

    /// The hour and the minute of an RFC 3339 time in this zone, or the text
    /// itself for a time of another form.
    pub fn minute(&self, time: &str) -> String {
        let local = self.local(time);
        local.map_or_else(
            || time.to_owned(),
            |local| local.strftime("%H:%M").to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Zone, bytes};

    #[test]
    fn a_size_takes_the_largest_unit_that_keeps_it_above_one() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(1536 * 1024 * 1024), "1.5 GiB");
        assert_eq!(bytes(u64::MAX), "16777216.0 TiB");
    }

    #[test]
    fn a_time_reads_in_its_zone() {
        let utc = Zone::utc();
        assert_eq!(utc.day("2026-11-20T01:14:01Z"), "2026-11-20");
        assert_eq!(utc.minute("2026-11-20T01:14:01Z"), "01:14");
        // Five hours west of UTC, the same instant falls on the day before.
        let west = Zone::fixed(-5);
        assert_eq!(west.day("2026-11-20T01:14:01Z"), "2026-11-19");
        assert_eq!(west.minute("2026-11-20T01:14:01Z"), "20:14");
        assert_eq!(Zone::fixed(9).minute("2026-11-20T23:30:00Z"), "08:30");
        // The instant 1795219200 is 2026-11-21T00:00:00Z.
        assert_eq!(utc.day_at(1_795_219_200), "2026-11-21");
        assert_eq!(west.day_at(1_795_219_200), "2026-11-20");
        assert_eq!(utc.day_at(i64::MAX), "");
    }

    #[test]
    fn a_time_of_another_form_has_no_day_and_keeps_its_text() {
        assert_eq!(Zone::utc().day("soon"), "");
        assert_eq!(Zone::utc().minute("soon"), "soon");
    }
}
