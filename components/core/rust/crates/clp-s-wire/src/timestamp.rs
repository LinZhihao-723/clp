//! Timestamp units on the wire.

/// Converts an epoch-nanosecond timestamp to the milliseconds written on the wire, rounding
/// toward negative infinity (DV-27: C++ truncates toward zero, so only pre-1970 values differ).
///
/// # Returns
///
/// The timestamp in epoch milliseconds.
#[must_use]
pub const fn epoch_ns_to_wire_ms(epoch_ns: i64) -> i64 {
    epoch_ns.div_euclid(1_000_000)
}

#[cfg(test)]
mod tests {
    use super::epoch_ns_to_wire_ms;

    #[test]
    fn wire_milliseconds_round_toward_negative_infinity() {
        assert_eq!(
            epoch_ns_to_wire_ms(1_735_694_600_123_999_999),
            1_735_694_600_123
        );
        assert_eq!(epoch_ns_to_wire_ms(0), 0);
        assert_eq!(epoch_ns_to_wire_ms(999_999), 0);
        assert_eq!(epoch_ns_to_wire_ms(-1), -1);
        assert_eq!(epoch_ns_to_wire_ms(-1_000_000), -1);
        assert_eq!(epoch_ns_to_wire_ms(-1_000_001), -2);
        assert_eq!(
            epoch_ns_to_wire_ms(i64::MIN),
            i64::MIN.div_euclid(1_000_000)
        );
    }
}
