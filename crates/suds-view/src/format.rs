//! The text of a size and of a time, as every view writes them.

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

/// The date of an RFC 3339 time, or nothing for a time of another form.
pub fn day(time: &str) -> &str {
    time.get(..10).filter(|date| date.is_ascii()).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::{bytes, day};

    #[test]
    fn a_size_takes_the_largest_unit_that_keeps_it_above_one() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(1536 * 1024 * 1024), "1.5 GiB");
        assert_eq!(bytes(u64::MAX), "16777216.0 TiB");
    }

    #[test]
    fn a_day_is_the_date_of_a_time() {
        assert_eq!(day("2026-11-20T01:14:01Z"), "2026-11-20");
        assert_eq!(day("soon"), "");
    }
}
