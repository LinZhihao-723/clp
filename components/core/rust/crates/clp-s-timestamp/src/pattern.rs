//! Compiled timestamp patterns.

use crate::TimestampError;
use crate::calendar::DateTime;
use crate::calendar::NANOSECONDS_PER_SECOND;

/// A compiled 0.5.x timestamp pattern.
///
/// The writer stores two kinds of patterns: quoted patterns (`"..."`, for timestamps that were JSON
/// strings) and unquoted epoch patterns (for timestamps that were JSON numbers).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimestampPattern {
    pattern: Box<str>,
    fields: Box<[Field]>,
    is_quoted: bool,
    is_date_time: bool,
    timezone_offset_minutes: i32,
}

impl TimestampPattern {
    /// Factory function.
    ///
    /// Compiles a pattern as stored in an archive's timestamp dictionary, accepting exactly the
    /// patterns C++ `TimestampPattern::create` accepts, except the capture-and-transform sequences
    /// (`\Z`, `\?`, `\P`, `\O{...}`, `\s`), which C++ accepts but cannot format.
    ///
    /// # Returns
    ///
    /// The compiled pattern on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TimestampError::UnsupportedSpecifier`] if the pattern uses a specifier this reader does
    ///   not know, or a capture-and-transform sequence.
    /// * [`TimestampError::InvalidPattern`] if the pattern is malformed.
    pub fn parse(pattern: &str) -> Result<Self, TimestampError> {
        Parser::new(pattern).parse()
    }

    /// # Returns
    ///
    /// The pattern text as stored in the archive.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.pattern
    }

    /// # Returns
    ///
    /// Whether values formatted with this pattern are JSON numbers (epoch patterns) rather than
    /// JSON strings: C++ marshals values of an unquoted pattern as a bare JSON literal and values
    /// of a quoted pattern as a JSON string.
    #[must_use]
    pub const fn is_numeric(&self) -> bool {
        !self.is_quoted
    }

    /// Appends `epoch_ns` formatted with this pattern to `output`, unquoted and unescaped.
    ///
    /// On failure, `output` is left unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TimestampError::Unformattable`] if `epoch_ns` cannot be represented by the pattern.
    pub fn format(&self, epoch_ns: i64, output: &mut Vec<u8>) -> Result<(), TimestampError> {
        let original_length = output.len();
        self.append_fields(epoch_ns, false, output)
            .inspect_err(|_| output.truncate(original_length))
    }

    /// Appends `epoch_ns` formatted with this pattern to `output` exactly as C++
    /// `marshal_timestamp` does: the surrounding quotes of a quoted pattern are kept and `\\` is
    /// written as the JSON escape `\\`, so the text is the JSON literal C++ prints.
    ///
    /// On failure, `output` is left unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TimestampError::Unformattable`] if `epoch_ns` cannot be represented by the pattern.
    pub fn format_json(&self, epoch_ns: i64, output: &mut Vec<u8>) -> Result<(), TimestampError> {
        let original_length = output.len();
        if self.is_quoted {
            output.push(b'"');
        }
        let result = self.append_fields(epoch_ns, true, output);
        if result.is_err() {
            output.truncate(original_length);
        } else if self.is_quoted {
            output.push(b'"');
        }
        result
    }

    /// Appends every field of the pattern formatted for `epoch_ns` to `output`; on failure,
    /// `output` may hold a partial value.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TimestampError::Unformattable`] if a field cannot represent `epoch_ns`.
    fn append_fields(
        &self,
        epoch_ns: i64,
        should_escape_backslash: bool,
        output: &mut Vec<u8>,
    ) -> Result<(), TimestampError> {
        let unformattable = || TimestampError::Unformattable {
            pattern: self.pattern.clone(),
            epoch_ns,
        };
        let date_time = self
            .is_date_time
            .then(|| DateTime::from_epoch_ns(epoch_ns, self.timezone_offset_minutes));
        // Date-time patterns format sub-seconds of the (floored) time of day; numeric patterns
        // format the magnitude of the truncated remainder, as C++ does for each.
        let subsecond_nanoseconds = date_time.map_or_else(
            || (epoch_ns % NANOSECONDS_PER_SECOND).unsigned_abs(),
            |date_time| u64::from(date_time.subsecond_nanoseconds),
        );

        for field in &self.fields {
            match field {
                Field::Literal(text) | Field::Timezone(text) => output.extend_from_slice(text),
                Field::Backslash => {
                    if should_escape_backslash {
                        output.push(b'\\');
                    }
                    output.push(b'\\');
                }
                Field::EpochSeconds => append_integer(epoch_ns / NANOSECONDS_PER_SECOND, output),
                Field::EpochMilliseconds => append_integer(epoch_ns / 1_000_000, output),
                Field::EpochMicroseconds => append_integer(epoch_ns / 1_000, output),
                Field::EpochNanoseconds => append_integer(epoch_ns, output),
                Field::Milliseconds => {
                    append_padded(subsecond_nanoseconds / 1_000_000, 3, b'0', output);
                }
                Field::Microseconds => {
                    append_padded(subsecond_nanoseconds / 1_000, 6, b'0', output);
                }
                Field::Nanoseconds => append_padded(subsecond_nanoseconds, 9, b'0', output),
                Field::TrimmedFraction => {
                    if 0 == subsecond_nanoseconds {
                        return Err(unformattable());
                    }
                    let mut digits = Vec::with_capacity(9);
                    append_padded(subsecond_nanoseconds, 9, b'0', &mut digits);
                    while digits.last() == Some(&b'0') {
                        digits.pop();
                    }
                    output.extend_from_slice(&digits);
                }
                date_time_field => {
                    let date_time = date_time.ok_or_else(unformattable)?;
                    append_date_time_field(date_time_field, &date_time, output)
                        .ok_or_else(unformattable)?;
                }
            }
        }
        Ok(())
    }
}

const NUM_MONTHS: usize = 12;
const NUM_WEEKDAYS: usize = 7;
const TWO_DIGIT_YEAR_LOW_OFFSET: i64 = 1900;
const TWO_DIGIT_YEAR_HIGH_OFFSET: i64 = 2000;
const LEAP_SECOND: i64 = 60;
const MAX_TIMEZONE_OFFSET_HOUR: i32 = 23;
const MAX_TIMEZONE_OFFSET_MINUTE: i32 = 59;
const UNICODE_MINUS_SIGN: &[u8] = "\u{2212}".as_bytes();

/// One compiled element of a pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Field {
    /// Literal text, copied verbatim.
    Literal(Box<[u8]>),

    /// `\\`.
    Backslash,

    /// `\y`.
    TwoDigitYear,

    /// `\Y`.
    Year,

    /// `\B{...}`, with the twelve month names.
    MonthName(Box<[Box<[u8]>]>),

    /// `\m`.
    Month,

    /// `\d`.
    ZeroPaddedDay,

    /// `\e`.
    SpacePaddedDay,

    /// `\A{...}`, with the seven weekday names starting from Sunday.
    WeekdayName(Box<[Box<[u8]>]>),

    /// `\p`.
    PartOfDay,

    /// `\H`.
    ZeroPaddedHour,

    /// `\k`.
    SpacePaddedHour,

    /// `\I`.
    ZeroPaddedTwelveHourClockHour,

    /// `\l`.
    SpacePaddedTwelveHourClockHour,

    /// `\M`.
    Minute,

    /// `\S`.
    Second,

    /// `\J`.
    LeapSecond,

    /// `\3`.
    Milliseconds,

    /// `\6`.
    Microseconds,

    /// `\9`.
    Nanoseconds,

    /// `\T`.
    TrimmedFraction,

    /// `\z{...}` or `\o{...,...}`, with the text it prints.
    Timezone(Box<[u8]>),

    /// `\E`.
    EpochSeconds,

    /// `\L`.
    EpochMilliseconds,

    /// `\C`.
    EpochMicroseconds,

    /// `\N`.
    EpochNanoseconds,
}

/// The state of compiling one pattern; mirrors the loop of C++ `TimestampPattern::create`.
struct Parser<'pattern> {
    pattern: &'pattern str,
    bytes: &'pattern [u8],
    fields: Vec<Field>,
    literal: Vec<u8>,
    seen_specifiers: [bool; 256],
    timezone_offset_minutes: Option<i32>,
    uses_date_time: bool,
    uses_epoch: bool,
}

impl<'pattern> Parser<'pattern> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A parser for `pattern`.
    const fn new(pattern: &'pattern str) -> Self {
        Self {
            pattern,
            bytes: pattern.as_bytes(),
            fields: Vec::new(),
            literal: Vec::new(),
            seen_specifiers: [false; 256],
            timezone_offset_minutes: None,
            uses_date_time: false,
            uses_epoch: false,
        }
    }

    /// Compiles the pattern.
    ///
    /// # Returns
    ///
    /// The compiled pattern on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TimestampError::InvalidPattern`] if the pattern contains a control character or `"`,
    ///   ends with a lone `\`, mixes date-time and epoch specifiers, or has a 12-hour clock without
    ///   a part of day (or the reverse).
    /// * Forwards [`Self::parse_specifier`]'s return values on failure.
    fn parse(mut self) -> Result<TimestampPattern, TimestampError> {
        let length = self.bytes.len();
        let is_quoted = length >= 2 && b'"' == self.bytes[0] && b'"' == self.bytes[length - 1];
        let (begin, end) = if is_quoted {
            (1, length - 1)
        } else {
            (0, length)
        };

        let mut index = begin;
        let mut is_escaped = false;
        while index < end {
            let byte = self.bytes[index];
            if b'"' == byte || byte <= 0x1f {
                return Err(self.invalid("unsupported character"));
            }
            if is_escaped {
                is_escaped = false;
                index = self.parse_specifier(index)?;
            } else if b'\\' == byte {
                is_escaped = true;
            } else {
                self.literal.push(byte);
            }
            index += 1;
        }

        if is_escaped {
            return Err(self.invalid("trailing escape"));
        }
        if self.uses_date_time && self.uses_epoch {
            return Err(self.invalid("date-time and epoch specifiers are mixed"));
        }
        let uses_twelve_hour_clock = self.is_seen(b'I') || self.is_seen(b'l');
        if uses_twelve_hour_clock != self.is_seen(b'p') {
            return Err(self.invalid("a 12-hour clock and a part of day must appear together"));
        }
        self.flush_literal();
        Ok(TimestampPattern {
            pattern: self.pattern.into(),
            fields: self.fields.into(),
            is_quoted,
            is_date_time: self.uses_date_time,
            timezone_offset_minutes: self.timezone_offset_minutes.unwrap_or(0),
        })
    }

    /// Compiles the specifier at `index` (the byte after a `\`).
    ///
    /// # Returns
    ///
    /// The index of the specifier's last byte (its closing `}` for bracketed specifiers) on
    /// success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TimestampError::UnsupportedSpecifier`] if the specifier is unknown, a JSON escape, or a
    ///   capture-and-transform sequence.
    /// * [`TimestampError::InvalidPattern`] if a non-repeatable specifier repeats, a second
    ///   timezone appears, or a bracketed specifier's content is invalid.
    /// * Forwards [`Self::bracket`]'s return values on failure.
    /// * Forwards [`Self::bracket_list`]'s return values on failure.
    /// * Forwards [`Self::set_timezone`]'s return values on failure.
    fn parse_specifier(&mut self, index: usize) -> Result<usize, TimestampError> {
        let specifier = self.bytes[index];
        if matches!(specifier, b'b' | b'f' | b'n' | b'r' | b't' | b'u') {
            return Err(self.unsupported(index));
        }
        if b'\\' != specifier && b'O' != specifier {
            let seen = &mut self.seen_specifiers[usize::from(specifier)];
            if *seen {
                return Err(self.invalid("a specifier repeats"));
            }
            *seen = true;
        }

        let mut last_index = index;
        let field = match specifier {
            b'y' => Field::TwoDigitYear,
            b'Y' => Field::Year,
            b'B' => {
                let content = self.bracket(index)?;
                last_index += content.len() + 2;
                let names = self.bracket_list(content)?;
                if NUM_MONTHS != names.len() {
                    return Err(self.invalid("month names are not twelve"));
                }
                Field::MonthName(names)
            }
            b'm' => Field::Month,
            b'd' => Field::ZeroPaddedDay,
            b'e' => Field::SpacePaddedDay,
            b'A' => {
                let content = self.bracket(index)?;
                last_index += content.len() + 2;
                let names = self.bracket_list(content)?;
                if NUM_WEEKDAYS != names.len() {
                    return Err(self.invalid("weekday names are not seven"));
                }
                Field::WeekdayName(names)
            }
            b'p' => Field::PartOfDay,
            b'H' => Field::ZeroPaddedHour,
            b'k' => Field::SpacePaddedHour,
            b'I' => Field::ZeroPaddedTwelveHourClockHour,
            b'l' => Field::SpacePaddedTwelveHourClockHour,
            b'M' => Field::Minute,
            b'S' => Field::Second,
            b'J' => Field::LeapSecond,
            b'3' => Field::Milliseconds,
            b'6' => Field::Microseconds,
            b'9' => Field::Nanoseconds,
            b'T' => Field::TrimmedFraction,
            b'E' => Field::EpochSeconds,
            b'L' => Field::EpochMilliseconds,
            b'C' => Field::EpochMicroseconds,
            b'N' => Field::EpochNanoseconds,
            b'z' => {
                let content = self.bracket(index)?;
                last_index += content.len() + 2;
                self.set_timezone(content)?;
                Field::Timezone(content.into())
            }
            b'o' => {
                let content = self.bracket(index)?;
                last_index += content.len() + 2;
                let [name, offset]: [Box<[u8]>; 2] = self
                    .bracket_list(content)?
                    .into_vec()
                    .try_into()
                    .map_err(|_| self.invalid("a named timezone is not a name and an offset"))?;
                self.set_timezone(&offset)?;
                Field::Timezone(name)
            }
            b'\\' => Field::Backslash,
            _ => return Err(self.unsupported(index)),
        };

        match field {
            Field::Backslash
            | Field::Milliseconds
            | Field::Microseconds
            | Field::Nanoseconds
            | Field::TrimmedFraction => {}
            Field::EpochSeconds
            | Field::EpochMilliseconds
            | Field::EpochMicroseconds
            | Field::EpochNanoseconds => self.uses_epoch = true,
            _ => self.uses_date_time = true,
        }
        self.flush_literal();
        self.fields.push(field);
        Ok(last_index)
    }

    /// Records the timezone given by `offset_text`, which must be exactly one offset.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TimestampError::InvalidPattern`] if a timezone was already given, or `offset_text` is
    ///   not exactly one offset.
    /// * Forwards [`Self::timezone_offset`]'s return values on failure.
    fn set_timezone(&mut self, offset_text: &[u8]) -> Result<(), TimestampError> {
        if self.timezone_offset_minutes.is_some() {
            return Err(self.invalid("a second timezone appears"));
        }
        let (consumed_length, offset_minutes) = self.timezone_offset(offset_text)?;
        if consumed_length != offset_text.len() {
            return Err(self.invalid("a timezone has trailing text"));
        }
        self.timezone_offset_minutes = Some(offset_minutes);
        Ok(())
    }

    /// Extracts the `{...}` content after the specifier at `index` (`extract_bracket_pattern`).
    ///
    /// # Returns
    ///
    /// The bytes between the brackets on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TimestampError::InvalidPattern`] if no `{` follows, no `}` closes it, the content is
    ///   empty, or the content contains `\`.
    fn bracket(&self, index: usize) -> Result<&'pattern [u8], TimestampError> {
        let rest = &self.bytes[index + 1..];
        if rest.first() != Some(&b'{') {
            return Err(self.invalid("a bracketed specifier has no `{`"));
        }
        let Some(closing) = rest.iter().position(|&byte| b'}' == byte || b'\\' == byte) else {
            return Err(self.invalid("a bracketed specifier has no `}`"));
        };
        if b'\\' == rest[closing] || 1 == closing {
            return Err(self.invalid("a bracketed specifier is empty or contains `\\`"));
        }
        Ok(&rest[1..closing])
    }

    /// Splits bracket content at `,` (`extract_bracket_pattern_list`).
    ///
    /// # Returns
    ///
    /// The elements on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TimestampError::InvalidPattern`] if the content exceeds 65,535 bytes, or an element is
    ///   empty or contains a space or `\`.
    fn bracket_list(&self, content: &[u8]) -> Result<Box<[Box<[u8]>]>, TimestampError> {
        if content.len() > usize::from(u16::MAX) {
            return Err(self.invalid("a bracketed list is too long"));
        }
        content
            .split(|&byte| b',' == byte)
            .map(|element| {
                if element.is_empty() || element.iter().any(|&byte| b' ' == byte || b'\\' == byte) {
                    Err(self.invalid("a bracketed list element is empty or invalid"))
                } else {
                    Ok(element.into())
                }
            })
            .collect()
    }

    /// Parses a timezone offset prefix of `text` (`extract_timezone_offset_in_minutes`): a sign
    /// (`+`, `-` or U+2212), two hour digits, then optionally `:` and two minute digits.
    ///
    /// # Returns
    ///
    /// The length of the offset prefix and the offset in minutes on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TimestampError::InvalidPattern`] if `text` does not start with a valid offset.
    fn timezone_offset(&self, text: &[u8]) -> Result<(usize, i32), TimestampError> {
        let invalid = || self.invalid("invalid timezone offset");
        let (sign, mut length) = if text.starts_with(b"+") {
            (1, 1)
        } else if text.starts_with(b"-") {
            (-1, 1)
        } else if text.starts_with(UNICODE_MINUS_SIGN) {
            (-1, UNICODE_MINUS_SIGN.len())
        } else {
            return Err(invalid());
        };

        let hours = text
            .get(length..length + 2)
            .and_then(|field| parse_padded_number(field, b'0'))
            .filter(|hours| (0..=MAX_TIMEZONE_OFFSET_HOUR).contains(hours))
            .ok_or_else(invalid)?;
        length += 2;
        let hours_only = (length, sign * hours * 60);

        let minutes_begin = if text.get(length) == Some(&b':') {
            length + 1
        } else {
            length
        };
        let minutes = text
            .get(minutes_begin..minutes_begin + 2)
            .and_then(|field| parse_padded_number(field, b'0'))
            .filter(|minutes| (0..=MAX_TIMEZONE_OFFSET_MINUTE).contains(minutes));
        Ok(minutes.map_or(hours_only, |minutes| {
            (minutes_begin + 2, sign * (hours * 60 + minutes))
        }))
    }

    /// # Returns
    ///
    /// Whether the pattern used the specifier `\specifier` so far.
    const fn is_seen(&self, specifier: u8) -> bool {
        self.seen_specifiers[specifier as usize]
    }

    /// Moves the pending literal text into a field.
    fn flush_literal(&mut self) {
        if !self.literal.is_empty() {
            let literal = std::mem::take(&mut self.literal);
            self.fields.push(Field::Literal(literal.into()));
        }
    }

    /// # Returns
    ///
    /// An [`TimestampError::InvalidPattern`] for this pattern.
    fn invalid(&self, reason: &'static str) -> TimestampError {
        TimestampError::InvalidPattern {
            pattern: self.pattern.into(),
            reason,
        }
    }

    /// # Returns
    ///
    /// An [`TimestampError::UnsupportedSpecifier`] for the specifier at `index`, reported at the
    /// position of its `\`.
    fn unsupported(&self, index: usize) -> TimestampError {
        TimestampError::UnsupportedSpecifier {
            pattern: self.pattern.into(),
            position: index - 1,
        }
    }
}

/// Appends the date-time field `field` of `date_time` to `output`.
///
/// # Returns
///
/// `Some(())` on success, or `None` if the field cannot represent `date_time` (a year before 1900
/// in `\y`, or a negative year) or is not a date-time field.
fn append_date_time_field(field: &Field, date_time: &DateTime, output: &mut Vec<u8>) -> Option<()> {
    let twelve_hour_clock_hour = match date_time.hour % 12 {
        0 => 12,
        hour => hour,
    };
    match field {
        Field::TwoDigitYear => {
            let offset = if date_time.year >= TWO_DIGIT_YEAR_HIGH_OFFSET {
                TWO_DIGIT_YEAR_HIGH_OFFSET
            } else {
                TWO_DIGIT_YEAR_LOW_OFFSET
            };
            append_padded(
                u64::try_from(date_time.year - offset).ok()?,
                2,
                b'0',
                output,
            );
        }
        Field::Year => append_padded(u64::try_from(date_time.year).ok()?, 4, b'0', output),
        Field::MonthName(names) => {
            output.extend_from_slice(&names[usize::from(date_time.month) - 1]);
        }
        Field::Month => append_padded(u64::from(date_time.month), 2, b'0', output),
        Field::ZeroPaddedDay => append_padded(u64::from(date_time.day), 2, b'0', output),
        Field::SpacePaddedDay => append_padded(u64::from(date_time.day), 2, b' ', output),
        Field::WeekdayName(names) => {
            output.extend_from_slice(&names[usize::from(date_time.weekday)]);
        }
        Field::PartOfDay => {
            output.extend_from_slice(if date_time.hour >= 12 { b"PM" } else { b"AM" });
        }
        Field::ZeroPaddedHour => append_padded(u64::from(date_time.hour), 2, b'0', output),
        Field::SpacePaddedHour => append_padded(u64::from(date_time.hour), 2, b' ', output),
        Field::ZeroPaddedTwelveHourClockHour => {
            append_padded(u64::from(twelve_hour_clock_hour), 2, b'0', output);
        }
        Field::SpacePaddedTwelveHourClockHour => {
            append_padded(u64::from(twelve_hour_clock_hour), 2, b' ', output);
        }
        Field::Minute => append_padded(u64::from(date_time.minute), 2, b'0', output),
        Field::Second => append_padded(u64::from(date_time.second), 2, b'0', output),
        Field::LeapSecond => append_integer(LEAP_SECOND, output),
        _ => return None,
    }
    Some(())
}

/// Appends `value` in decimal to `output`.
fn append_integer(value: i64, output: &mut Vec<u8>) {
    output.extend_from_slice(itoa::Buffer::new().format(value).as_bytes());
}

/// Appends `value` in decimal to `output`, left-padded with `padding` to at least `min_length`
/// bytes (`append_positive_left_padded_integer`).
fn append_padded(value: u64, min_length: usize, padding: u8, output: &mut Vec<u8>) {
    let mut buffer = itoa::Buffer::new();
    let digits = buffer.format(value).as_bytes();
    output.extend(std::iter::repeat_n(
        padding,
        min_length.saturating_sub(digits.len()),
    ));
    output.extend_from_slice(digits);
}

/// Parses a padded number the way C++ `convert_padded_string_to_number` does: leading `padding`
/// bytes are stripped (keeping at least one byte), a remaining leading zero is rejected, and the
/// rest must be an optionally negative decimal `i32`.
///
/// # Returns
///
/// The number, or `None` if `text` is not a padded number.
fn parse_padded_number(text: &[u8], padding: u8) -> Option<i32> {
    let num_padding = text
        .iter()
        .take(text.len().checked_sub(1)?)
        .take_while(|&&byte| padding == byte)
        .count();
    let stripped = &text[num_padding..];
    if stripped.len() > 1 && b'0' == stripped[0] {
        return None;
    }
    let (is_negative, digits) = match stripped.split_first() {
        Some((b'-', digits)) => (true, digits),
        _ => (false, stripped),
    };
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0_i32, |value, &byte| {
        let digit = i32::from(byte.is_ascii_digit().then(|| byte - b'0')?);
        let value = value.checked_mul(10)?;
        if is_negative {
            value.checked_sub(digit)
        } else {
            value.checked_add(digit)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::TimestampPattern;
    use crate::TimestampError;

    /// # Returns
    ///
    /// `epoch_ns` formatted with `pattern` by [`TimestampPattern::format`].
    ///
    /// # Panics
    ///
    /// Panics if the pattern does not compile or cannot format `epoch_ns`.
    fn formatted(pattern: &str, epoch_ns: i64) -> String {
        let mut output = Vec::new();
        TimestampPattern::parse(pattern)
            .expect("the pattern should compile")
            .format(epoch_ns, &mut output)
            .expect("the timestamp should be formattable");
        String::from_utf8(output).expect("formatted timestamps of these patterns are UTF-8")
    }

    /// # Returns
    ///
    /// `epoch_ns` formatted with `pattern` by [`TimestampPattern::format_json`].
    ///
    /// # Panics
    ///
    /// Panics if the pattern does not compile or cannot format `epoch_ns`.
    fn formatted_json(pattern: &str, epoch_ns: i64) -> String {
        let mut output = Vec::new();
        TimestampPattern::parse(pattern)
            .expect("the pattern should compile")
            .format_json(epoch_ns, &mut output)
            .expect("the timestamp should be formattable");
        String::from_utf8(output).expect("formatted timestamps of these patterns are UTF-8")
    }

    /// # Returns
    ///
    /// The error of compiling `pattern`.
    ///
    /// # Panics
    ///
    /// Panics if the pattern compiles.
    fn parse_error(pattern: &str) -> TimestampError {
        TimestampPattern::parse(pattern).expect_err("the pattern should be rejected")
    }

    #[test]
    fn quoted_patterns_are_strings_and_unquoted_patterns_are_numbers() {
        for (pattern, is_numeric) in [
            (r"\L", true),
            (r"\E.\3", true),
            (r#""\E""#, false),
            (r#""\Y-\m-\d""#, false),
            (r"\Y-\m-\d", true),
            ("\"\"", false),
            ("\"", false),
        ] {
            let result = TimestampPattern::parse(pattern);
            if "\"" == pattern {
                assert!(
                    matches!(result, Err(TimestampError::InvalidPattern { .. })),
                    "{result:?}"
                );
                continue;
            }
            let compiled = result.expect("the pattern should compile");
            assert_eq!(compiled.is_numeric(), is_numeric, "pattern {pattern}");
            assert_eq!(compiled.as_str(), pattern);
        }
        assert_eq!(formatted_json("\"\"", 1), "\"\"");
    }

    #[test]
    fn date_time_fields_format_like_cpp() {
        let midnight = 1_700_006_400_000_000_000;
        let hour_ns = 3_600_000_000_000;
        assert_eq!(formatted(r"\I \l \p", midnight), "12 12 AM");
        assert_eq!(formatted(r"\I \l \p", midnight + 9 * hour_ns), "09  9 AM");
        assert_eq!(formatted(r"\I \l \p", midnight + 12 * hour_ns), "12 12 PM");
        assert_eq!(formatted(r"\I \l \p", midnight + 13 * hour_ns), "01  1 PM");
        assert_eq!(formatted(r"\H \k", midnight + 7 * hour_ns), "07  7");
        assert_eq!(formatted(r"\d \e", 0), "01  1");
        assert_eq!(formatted(r"\M:\S:\J", 59_000_000_000), "00:59:60");
        assert_eq!(formatted(r"\y", -1), "69");
        assert_eq!(formatted(r"\y", 3_124_223_999_000_000_000), "68");
        assert_eq!(formatted(r"\Y", i64::MIN), "1677");
        assert_eq!(
            formatted(r"\Y-\m-\dT\H:\M:\S.\9", -1),
            "1969-12-31T23:59:59.999999999"
        );
        assert_eq!(
            formatted(r"\3 \6 \9 \T", 1_002_003),
            "001 001002 001002003 001002003"
        );
        assert_eq!(formatted(r"\Y\T", 120_000_000), "197012");
        assert_eq!(
            formatted(
                r"\A{Su,Mo,Tu,We,Th,Fr,Sa} \B{1,2,3,4,5,6,7,8,9,10,11,12}",
                0
            ),
            "Th 1"
        );
        assert_eq!(formatted(r"\H:\M \o{PST,-0800}", 0), "16:00 PST");
        assert_eq!(
            formatted("\\H:\\M \\z{\u{2212}01:30}", 0),
            "22:30 \u{2212}01:30"
        );
        assert_eq!(formatted(r"\H \z{+05}", 0), "05 +05");
        assert_eq!(formatted(r"a\\\\b", 0), r"a\\b");
    }

    #[test]
    fn numeric_fields_use_truncated_magnitudes() {
        assert_eq!(formatted(r"\E.\3", -1_500_000_000), "-1.500");
        assert_eq!(formatted(r"\E.\9", -1), "0.000000001");
        assert_eq!(
            formatted(r"\L \C \N", -1_234_567_891),
            "-1234 -1234567 -1234567891"
        );
        assert_eq!(formatted(r"\E", i64::MAX), "9223372036");
        assert_eq!(formatted(r"\N", i64::MIN), "-9223372036854775808");
        assert_eq!(formatted(r"\E.\T", -100_000_000), "0.1");
    }

    #[test]
    fn unformattable_values_are_errors_and_leave_output_unchanged() {
        for (pattern, epoch_ns) in [
            (r"\E.\T", 1_000_000_000),
            (r#""\Y-\m-\d \H:\M:\S.\T""#, 0),
            (r"\y", -2_208_988_800_000_000_001),
        ] {
            let compiled = TimestampPattern::parse(pattern).expect("the pattern should compile");
            for is_json in [false, true] {
                let mut output = b"kept".to_vec();
                let result = if is_json {
                    compiled.format_json(epoch_ns, &mut output)
                } else {
                    compiled.format(epoch_ns, &mut output)
                };
                assert!(
                    matches!(
                        result,
                        Err(TimestampError::Unformattable { epoch_ns: reported, .. })
                            if reported == epoch_ns
                    ),
                    "pattern {pattern}: {result:?}"
                );
                assert_eq!(output, b"kept");
            }
        }
        assert_eq!(formatted(r"\y", -2_208_988_800_000_000_000), "00");
    }

    #[test]
    fn unsupported_specifiers_are_rejected() {
        for (pattern, expected_position) in [
            (r"\q", 0),
            (r"ab\n", 2),
            (r"\b", 0),
            (r"\u0041", 0),
            (r"\Y-\m-\d \H:\M:\s", 15),
            (r"\Y-\m-\d \H:\M:\S\Z", 17),
            (r"\E.\?", 3),
            (r"\P", 0),
            (r"\Y\O{-/}\m", 2),
            (r#""\x""#, 1),
        ] {
            let error = parse_error(pattern);
            assert!(
                matches!(
                    &error,
                    TimestampError::UnsupportedSpecifier { position, .. }
                        if *position == expected_position
                ),
                "pattern {pattern}: {error:?}"
            );
        }
    }

    #[test]
    fn malformed_patterns_are_rejected() {
        for pattern in [
            r"\Y\Y",
            r"\3\3",
            "\\Y\"",
            "\\Y\u{1}",
            "\"\\Y\u{1f}\"",
            r"\Y\",
            r#""\Y\""#,
            r"\Y\E",
            r"\H\L",
            r"\I:\M",
            r"\l",
            r"\H \p",
            r"\B{Jan,Feb}",
            r"\B{}",
            r"\B",
            r"\Bx",
            r"\B{Jan",
            r"\B{Jan,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec,}",
            r"\B{,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec}",
            r"\B{Jan, Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec}",
            r"\B{Ja\n,Feb,Mar,Apr,May,Jun,Jul,Aug,Sep,Oct,Nov,Dec}",
            r"\A{Sun,Mon,Tue,Wed,Thu,Fri}",
            r"\z{+0800x}",
            r"\z{0800}",
            r"\z{+24}",
            r"\z{+8}",
            r"\z{+08:6}",
            r"\z{+08:}",
            r"\z{+08}\o{EST,-0500}",
            r"\o{EST}",
            r"\o{EST,-0500,x}",
            r"\o{EST,0500}",
            r"\z{}",
        ] {
            let error = parse_error(pattern);
            assert!(
                matches!(error, TimestampError::InvalidPattern { .. }),
                "pattern {pattern}: {error:?}"
            );
        }
    }

    #[test]
    fn edge_case_timezones_parse_like_cpp() {
        assert_eq!(formatted(r"\H:\M \z{-0130}", 0), "22:30 -0130");
        assert_eq!(formatted(r"\H:\M \z{+00:00}", 0), "00:00 +00:00");
        assert_eq!(formatted(r"\H:\M \z{+23:59}", 0), "23:59 +23:59");
        assert_eq!(formatted(r"\H:\M \z{+-0}", 0), "00:00 +-0");
        assert_eq!(formatted(r"\H:\M \o{X,+01}", 0), "01:00 X");
    }
}
