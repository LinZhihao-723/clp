//! Timestamp patterns of format 0.5.x archives (the `\`-specifier patterns of
//! `clp_s/timestamp_parser`): parsing a pattern stored in an archive's timestamp dictionary, and
//! formatting an epoch-nanosecond timestamp back into the text the C++ `clp-s` would print.

mod calendar;
mod error;
mod pattern;

pub use error::TimestampError;
pub use pattern::TimestampPattern;
