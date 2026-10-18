//! Float writers: shortest round-trip plain floats (DV-01), the C++ `std::to_string` (`%f`)
//! emulation, and the C++ `restore_encoded_float` port for `FormattedFloat` values.
//!
//! Non-finite values are written as `null` by every writer (DV-02).

use std::fmt::Write;

/// Appends the shortest text that round-trips to `value` to `output`.
pub fn write_shortest(value: f64, output: &mut Vec<u8>) {
    if value.is_finite() {
        output.extend_from_slice(ryu::Buffer::new().format_finite(value).as_bytes());
    } else {
        output.extend_from_slice(NULL);
    }
}

/// Appends `value` as C++ `std::to_string(double)` prints it (`%f`: six fraction digits, rounded
/// half to even on the exact binary value) to `output`.
pub fn write_cpp_plain(value: f64, output: &mut Vec<u8>) {
    if value.is_finite() {
        write!(Appender(output), "{value:.6}").expect("appending to a vector never fails");
    } else {
        output.extend_from_slice(NULL);
    }
}

/// Appends `value` restored with the float format `format` to `output`, as C++
/// `restore_encoded_float` (`FloatFormatEncoding.cpp`) does: `%.{n-1}e` with `n` significant
/// digits, then either rewritten as a plain decimal or given the stored exponent case, sign and
/// minimum number of digits.
pub fn write_formatted(value: f64, format: u16, output: &mut Vec<u8>) {
    if !value.is_finite() {
        output.extend_from_slice(NULL);
        return;
    }
    let num_significant_digits =
        usize::from((format & NUM_SIGNIFICANT_DIGITS_MASK) >> NUM_SIGNIFICANT_DIGITS_POS) + 1;
    let mut scientific = ScientificText::default();
    write!(
        scientific,
        "{:.*e}",
        num_significant_digits - 1,
        value.abs()
    )
    .expect("a scientific float of at most 32 significant digits fits the buffer");
    let (mantissa, exponent) = scientific.split();
    if value.is_sign_negative() {
        output.push(b'-');
    }

    if 0 == format & SCIENTIFIC_NOTATION_ENABLED_BIT {
        write_decimal(mantissa, exponent, output);
        return;
    }

    output.extend_from_slice(mantissa);
    output.push(
        if SCIENTIFIC_NOTATION_UPPERCASE_E_FLAG == format & SCIENTIFIC_NOTATION_FLAG_MASK {
            b'E'
        } else {
            b'e'
        },
    );
    match format & EXPONENT_SIGN_FLAG_MASK {
        PLUS_EXPONENT_SIGN_FLAG => output.push(b'+'),
        MINUS_EXPONENT_SIGN_FLAG => output.push(b'-'),
        _ => {}
    }
    let num_exponent_digits =
        usize::from((format & NUM_EXPONENT_DIGITS_MASK) >> NUM_EXPONENT_DIGITS_POS) + 1;
    let mut digits = itoa::Buffer::new();
    let digits = digits.format(exponent.unsigned_abs()).as_bytes();
    output.extend(std::iter::repeat_n(
        b'0',
        num_exponent_digits.saturating_sub(digits.len()),
    ));
    output.extend_from_slice(digits);
}

const NULL: &[u8] = b"null";

const SCIENTIFIC_NOTATION_FLAG_POS: u16 = 14;
const SCIENTIFIC_NOTATION_ENABLED_BIT: u16 = 0b01 << SCIENTIFIC_NOTATION_FLAG_POS;
const SCIENTIFIC_NOTATION_FLAG_MASK: u16 = 0b11 << SCIENTIFIC_NOTATION_FLAG_POS;
const SCIENTIFIC_NOTATION_UPPERCASE_E_FLAG: u16 = 0b11 << SCIENTIFIC_NOTATION_FLAG_POS;
const EXPONENT_SIGN_FLAG_POS: u16 = 12;
const EXPONENT_SIGN_FLAG_MASK: u16 = 0b11 << EXPONENT_SIGN_FLAG_POS;
const PLUS_EXPONENT_SIGN_FLAG: u16 = 0b01 << EXPONENT_SIGN_FLAG_POS;
const MINUS_EXPONENT_SIGN_FLAG: u16 = 0b10 << EXPONENT_SIGN_FLAG_POS;
const NUM_EXPONENT_DIGITS_POS: u16 = 10;
const NUM_EXPONENT_DIGITS_MASK: u16 = 0b11 << NUM_EXPONENT_DIGITS_POS;
const NUM_SIGNIFICANT_DIGITS_POS: u16 = 5;
const NUM_SIGNIFICANT_DIGITS_MASK: u16 = 0b1_1111 << NUM_SIGNIFICANT_DIGITS_POS;

/// Appends formatted text to a byte vector.
struct Appender<'output>(&'output mut Vec<u8>);

impl Write for Appender<'_> {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

/// A stack buffer for one non-negative number printed with Rust's `{:e}` (`d.ddde[-]x`).
struct ScientificText {
    bytes: [u8; Self::CAPACITY],
    length: usize,
}

impl ScientificText {
    /// Fits 32 significant digits, the point, `e`, a sign and three exponent digits.
    const CAPACITY: usize = 40;

    /// # Returns
    ///
    /// The mantissa text and the exponent.
    ///
    /// # Panics
    ///
    /// Panics if the buffer does not hold Rust's `{:e}` text of a finite number.
    fn split(&self) -> (&[u8], i32) {
        let text = &self.bytes[..self.length];
        let exponent_position = text
            .iter()
            .position(|byte| b'e' == *byte)
            .expect("scientific text always has an exponent");
        let exponent = std::str::from_utf8(&text[exponent_position + 1..])
            .expect("scientific text is ASCII")
            .parse()
            .expect("a scientific exponent is a decimal integer");
        (&text[..exponent_position], exponent)
    }
}

impl Default for ScientificText {
    fn default() -> Self {
        Self {
            bytes: [0; Self::CAPACITY],
            length: 0,
        }
    }
}

impl Write for ScientificText {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        let end = self.length + text.len();
        self.bytes
            .get_mut(self.length..end)
            .ok_or(std::fmt::Error)?
            .copy_from_slice(text.as_bytes());
        self.length = end;
        Ok(())
    }
}

/// Appends the plain decimal form of the scientific number `mantissa` (`d` or `d.ddd`) times ten
/// to the `exponent` to `output`, as C++ `scientific_to_decimal` writes it: no exponent, every
/// mantissa digit kept, and no decimal point when the point falls after the last digit.
fn write_decimal(mantissa: &[u8], exponent: i32, output: &mut Vec<u8>) {
    let mut digits = [0_u8; ScientificText::CAPACITY];
    let mut num_digits = 0;
    for byte in mantissa.iter().copied().filter(|byte| b'.' != *byte) {
        digits[num_digits] = byte;
        num_digits += 1;
    }
    let digits = &digits[..num_digits];
    let decimal_position = i64::from(exponent) + 1;
    if decimal_position <= 0 {
        output.extend_from_slice(b"0.");
        output.extend(std::iter::repeat_n(
            b'0',
            usize::try_from(-decimal_position).unwrap_or(usize::MAX),
        ));
        output.extend_from_slice(digits);
        return;
    }
    let decimal_position = usize::try_from(decimal_position).unwrap_or(usize::MAX);
    if decimal_position < digits.len() {
        output.extend_from_slice(&digits[..decimal_position]);
        output.push(b'.');
        output.extend_from_slice(&digits[decimal_position..]);
    } else {
        output.extend_from_slice(digits);
        output.extend(std::iter::repeat_n(b'0', decimal_position - digits.len()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// # Returns
    ///
    /// The shortest round-trip text of `value`, as [`write_shortest`] writes it.
    fn shortest(value: f64) -> String {
        let mut output = Vec::new();
        write_shortest(value, &mut output);
        String::from_utf8(output).expect("floats are ASCII")
    }

    /// # Returns
    ///
    /// The C++ `%f`-based text of `value`, as [`write_cpp_plain`] writes it.
    fn cpp_plain(value: f64) -> String {
        let mut output = Vec::new();
        write_cpp_plain(value, &mut output);
        String::from_utf8(output).expect("floats are ASCII")
    }

    /// # Returns
    ///
    /// The text of `value` restored with `format`, as [`write_formatted`] writes it.
    fn formatted(value: f64, format: u16) -> String {
        let mut output = Vec::new();
        write_formatted(value, format, &mut output);
        String::from_utf8(output).expect("floats are ASCII")
    }

    /// Port of C++ `get_float_encoding` for the texts the tests use (valid float texts only).
    ///
    /// # Returns
    ///
    /// The format of `text`.
    fn encode_format(text: &str) -> u16 {
        let bytes = text.as_bytes();
        let first_digit = usize::from(b'-' == bytes[0]);
        let dot = text.find('.');
        let exponent = text.find(['e', 'E']).unwrap_or(text.len());
        let mut format = 0;
        if exponent < text.len() {
            format |= if b'E' == bytes[exponent] {
                SCIENTIFIC_NOTATION_UPPERCASE_E_FLAG
            } else {
                SCIENTIFIC_NOTATION_ENABLED_BIT
            };
            match bytes[exponent + 1] {
                b'+' => format |= PLUS_EXPONENT_SIGN_FLAG,
                b'-' => format |= MINUS_EXPONENT_SIGN_FLAG,
                _ => {}
            }
            let num_exponent_digits =
                text.len() - exponent - 1 - usize::from(!bytes[exponent + 1].is_ascii_digit());
            format |= u16::try_from(num_exponent_digits - 1).expect("few exponent digits")
                << NUM_EXPONENT_DIGITS_POS;
        }
        let first_non_zero = dot
            .filter(|_| b'0' == bytes[first_digit])
            .and_then(|dot| {
                bytes[dot + 1..exponent]
                    .iter()
                    .position(|byte| b'0' != *byte)
                    .map(|offset| dot + 1 + offset)
            })
            .unwrap_or(first_digit);
        let num_significant_digits =
            exponent - first_non_zero - usize::from(dot.is_some_and(|dot| first_non_zero < dot));
        format
            | u16::try_from(num_significant_digits - 1).expect("few significant digits")
                << NUM_SIGNIFICANT_DIGITS_POS
    }

    #[test]
    fn shortest_floats_round_trip_and_stay_floats() {
        for value in [
            0.0,
            -0.0,
            1.0,
            1.5,
            0.1,
            0.300_000_000_000_000_04,
            1e-7,
            1e16,
            1e300,
            f64::MAX,
            f64::MIN_POSITIVE,
            5e-324,
            -123_456.789,
        ] {
            let text = shortest(value);
            let parsed: f64 = text.parse().expect("the text is a float");
            assert_eq!(parsed.to_bits(), value.to_bits(), "{value:?} -> {text}");
            assert!(
                text.contains(['.', 'e']),
                "{value:?} -> {text} is not a float token"
            );
        }
    }

    #[test]
    fn non_finite_floats_are_null() {
        for value in [f64::NAN, -f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(shortest(value), "null", "{value:?}");
            assert_eq!(cpp_plain(value), "null", "{value:?}");
            assert_eq!(formatted(value, 0), "null", "{value:?}");
        }
    }

    #[test]
    fn cpp_plain_floats_match_percent_f() {
        let cases = [
            (1.5, "1.500000"),
            (-0.0, "-0.000000"),
            (1e-7, "0.000000"),
            (1e10, "10000000000.000000"),
            (0.000_000_6, "0.000001"),
            // Exact binary ties round half to even, like glibc.
            (0.007_812_5, "0.007812"),
            (0.023_437_5, "0.023438"),
            (-2.5, "-2.500000"),
            (1e22, "10000000000000000000000.000000"),
        ];
        for (value, expected) in cases {
            assert_eq!(cpp_plain(value), expected, "{value:?}");
        }
        assert!(
            cpp_plain(f64::MAX).starts_with("179769313486231570814527423731704356798070567525844"),
            "{}",
            cpp_plain(f64::MAX)
        );
    }

    #[test]
    fn formatted_floats_restore_their_original_text() {
        for text in [
            "1.5",
            "1.50",
            "0.1",
            "-0.0",
            "0.0",
            "1e10",
            "1E10",
            "1.5e+10",
            "1.5E-07",
            "1e-7",
            "123456.789",
            "1.7976931348623157e308",
            "5e-324",
            "0.000001",
            "100.0",
            "1.0e0",
            "1e-0",
            "-1.5e-300",
            "2.5e+005",
            "1E+2",
            "-7.25E-3",
            "0.30000000000000004",
            "1e300",
            "123.4560",
            "0.00000000012345",
            "-2.5",
            "6.02214076e23",
            "9.999e-1",
            "1.0000e0010",
        ] {
            let value: f64 = text.parse().expect("the text is a float");
            assert_eq!(
                formatted(value, encode_format(text)),
                text,
                "format {:#06x}",
                encode_format(text)
            );
        }
    }

    #[test]
    fn formatted_floats_round_half_to_even_like_printf() {
        let one_significant_digit = 0;
        let two_significant_digits = 1 << NUM_SIGNIFICANT_DIGITS_POS;
        let cases = [
            (2.5, one_significant_digit, "2"),
            (3.5, one_significant_digit, "4"),
            (0.125, two_significant_digits, "0.12"),
            (0.375, two_significant_digits, "0.38"),
            (9.5, one_significant_digit, "10"),
            (99.5, two_significant_digits, "100"),
        ];
        for (value, format, expected) in cases {
            assert_eq!(formatted(value, format), expected, "{value:?}");
        }
    }

    #[test]
    fn decimal_rewrites_keep_every_mantissa_digit() {
        let cases: [(&[u8], i32, &str); 5] = [
            (b"1.50", 0, "1.50"),
            (b"1.5", -3, "0.0015"),
            (b"1.5", 4, "15000"),
            (b"1", 2, "100"),
            (b"1.2345", 2, "123.45"),
        ];
        for (mantissa, exponent, expected) in cases {
            let mut output = Vec::new();
            write_decimal(mantissa, exponent, &mut output);
            assert_eq!(output, expected.as_bytes(), "{exponent}");
        }
    }
}
