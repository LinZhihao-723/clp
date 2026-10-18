//! Proleptic Gregorian calendar arithmetic over epoch nanoseconds (what C++ gets from
//! `date::year_month_day`, `date::year_month_weekday` and `date::make_time`).

pub const NANOSECONDS_PER_SECOND: i64 = 1_000_000_000;

/// The calendar fields of one instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTime {
    pub year: i64,

    /// 1 (January) to 12.
    pub month: u8,

    /// 1 to 31.
    pub day: u8,

    /// 0 (Sunday) to 6 (Saturday).
    pub weekday: u8,

    /// 0 to 23.
    pub hour: u8,

    /// 0 to 59.
    pub minute: u8,

    /// 0 to 59.
    pub second: u8,

    /// 0 to 999,999,999.
    pub subsecond_nanoseconds: u32,
}

impl DateTime {
    /// Factory function.
    ///
    /// Breaks `epoch_ns`, shifted by `offset_minutes`, into calendar fields; times before the
    /// epoch round towards negative infinity, as `date::floor<date::days>` does.
    ///
    /// # Returns
    ///
    /// The calendar fields of the shifted instant.
    #[must_use]
    pub fn from_epoch_ns(epoch_ns: i64, offset_minutes: i32) -> Self {
        let shifted_ns = i128::from(epoch_ns) + i128::from(offset_minutes) * NANOSECONDS_PER_MINUTE;
        let days = i64::try_from(shifted_ns.div_euclid(NANOSECONDS_PER_DAY))
            .expect("days of an `i64` nanosecond instant fit in `i64`");
        let time_of_day_ns = i64::try_from(shifted_ns.rem_euclid(NANOSECONDS_PER_DAY))
            .expect("a nanosecond time of day fits in `i64`");
        let (year, month, day) = civil_from_days(days);

        let seconds_of_day = time_of_day_ns / NANOSECONDS_PER_SECOND;
        Self {
            year,
            month,
            day,
            weekday: small_field((days + 4).rem_euclid(7)),
            hour: small_field(seconds_of_day / 3600),
            minute: small_field(seconds_of_day / 60 % 60),
            second: small_field(seconds_of_day % 60),
            subsecond_nanoseconds: u32::try_from(time_of_day_ns % NANOSECONDS_PER_SECOND)
                .expect("a subsecond nanosecond count fits in `u32`"),
        }
    }
}

const NANOSECONDS_PER_MINUTE: i128 = 60_000_000_000;
const NANOSECONDS_PER_DAY: i128 = 86_400_000_000_000;

/// Converts days since 1970-01-01 into a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`, the algorithm behind `date::year_month_day`).
///
/// # Returns
///
/// The `(year, month, day)` of the date.
fn civil_from_days(days: i64) -> (i64, u8, u8) {
    let shifted_days = days + 719_468;
    let era = shifted_days.div_euclid(146_097);
    let day_of_era = shifted_days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, small_field(month), small_field(day))
}

/// # Returns
///
/// `value` as a `u8`.
///
/// # Panics
///
/// Panics if `value` does not fit in `u8`; callers only pass calendar fields below 256.
fn small_field(value: i64) -> u8 {
    u8::try_from(value).expect("calendar fields fit in `u8`")
}

#[cfg(test)]
mod tests {
    use super::DateTime;
    use super::civil_from_days;

    #[test]
    fn civil_dates_match_known_days() {
        for (days, expected) in [
            (0, (1970, 1, 1)),
            (-1, (1969, 12, 31)),
            (59, (1970, 3, 1)),
            (10_957, (2000, 1, 1)),
            (11_017, (2000, 3, 1)),
            (11_016, (2000, 2, 29)),
            (19_675, (2023, 11, 14)),
            (-719_468, (0, 3, 1)),
            (-719_469, (0, 2, 29)),
            (2_932_896, (9999, 12, 31)),
            (-106_751, (1677, 9, 22)),
            (106_751, (2262, 4, 11)),
        ] {
            assert_eq!(civil_from_days(days), expected, "days {days}");
        }
    }

    #[test]
    fn instants_break_down_with_floor_semantics() {
        assert_eq!(
            DateTime::from_epoch_ns(-1, 0),
            DateTime {
                year: 1969,
                month: 12,
                day: 31,
                weekday: 3,
                hour: 23,
                minute: 59,
                second: 59,
                subsecond_nanoseconds: 999_999_999,
            }
        );
        assert_eq!(
            DateTime::from_epoch_ns(1_700_000_000_123_456_789, 8 * 60),
            DateTime {
                year: 2023,
                month: 11,
                day: 15,
                weekday: 3,
                hour: 6,
                minute: 13,
                second: 20,
                subsecond_nanoseconds: 123_456_789,
            }
        );
    }

    #[test]
    fn extreme_instants_do_not_overflow() {
        let earliest = DateTime::from_epoch_ns(i64::MIN, -(23 * 60 + 59));
        assert_eq!((earliest.year, earliest.month, earliest.day), (1677, 9, 20));
        let latest = DateTime::from_epoch_ns(i64::MAX, 23 * 60 + 59);
        assert_eq!((latest.year, latest.month, latest.day), (2262, 4, 12));
    }
}
