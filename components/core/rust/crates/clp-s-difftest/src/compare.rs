//! Comparison of a C++ output with a Rust output as canonical multisets.
//!
//! Mismatches the plain-float divergences explain are accepted (DV-01: C++ prints plain floats
//! with `%f`; DV-02: C++ prints non-finite floats as `nan`/`inf`, Rust as `null`).

use std::collections::HashMap;

use crate::canonical::INVALID_PREFIX;
use crate::canonical::JsonValue;
use crate::multiset::MultisetDiff;
use crate::multiset::RecordMultiset;
use crate::multiset::render_record;

/// The outcome of comparing an expected (C++) multiset with an actual (Rust) one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecordComparison {
    pub num_expected: u64,
    pub num_actual: u64,

    /// The exact canonical difference.
    pub exact: MultisetDiff,

    /// Mismatching (C++, Rust) record pairs that are equal under float normalization.
    pub accepted: Vec<AcceptedPair>,

    /// The part of [`Self::exact`] no rule explains.
    pub unexplained: MultisetDiff,
}

impl RecordComparison {
    /// Compares `expected` (C++) with `actual` (Rust): first exactly, then by pairing each
    /// remaining expected record with an actual record that is equal under float normalization.
    ///
    /// # Returns
    ///
    /// The comparison.
    #[must_use]
    pub fn compare(expected: &RecordMultiset, actual: &RecordMultiset) -> Self {
        let exact = expected.diff(actual);
        let mut comparison = Self {
            num_expected: expected.len(),
            num_actual: actual.len(),
            exact: exact.clone(),
            accepted: Vec::new(),
            unexplained: MultisetDiff::default(),
        };
        if exact.is_empty() {
            return comparison;
        }

        let mut candidates: HashMap<Vec<u8>, Vec<Candidate>> = HashMap::new();
        for (record, count) in &exact.unexpected {
            let Some(value) = JsonValue::parse(record) else {
                comparison
                    .unexplained
                    .unexpected
                    .push((record.clone(), *count));
                continue;
            };
            candidates
                .entry(shape_key(&value))
                .or_default()
                .push(Candidate {
                    record: record.clone(),
                    value,
                    remaining_count: *count,
                });
        }

        for (record, count) in &exact.missing {
            let Some(parsed) = parse_cpp_record(record) else {
                comparison
                    .unexplained
                    .missing
                    .push((record.clone(), *count));
                continue;
            };
            let group = candidates.get_mut(&shape_key(&parsed.value));
            let mut num_unmatched = *count;
            if let Some(group) = group {
                while num_unmatched > 0 {
                    let Some((index, reasons)) = find_equivalent(&parsed, group) else {
                        break;
                    };
                    let candidate = &mut group[index];
                    candidate.remaining_count -= 1;
                    num_unmatched -= 1;
                    comparison.accepted.push(AcceptedPair {
                        expected: record.clone(),
                        actual: candidate.record.clone(),
                        reasons,
                    });
                }
            }
            if num_unmatched > 0 {
                comparison
                    .unexplained
                    .missing
                    .push((record.clone(), num_unmatched));
            }
        }

        for candidate in candidates.into_values().flatten() {
            if candidate.remaining_count > 0 {
                comparison
                    .unexplained
                    .unexpected
                    .push((candidate.record, candidate.remaining_count));
            }
        }
        comparison.unexplained.missing.sort_unstable();
        comparison.unexplained.unexpected.sort_unstable();
        comparison
    }

    /// # Returns
    ///
    /// Whether the two multisets are byte-identical in canonical form.
    #[must_use]
    pub const fn is_exact(&self) -> bool {
        self.exact.is_empty()
    }

    /// # Returns
    ///
    /// Whether every mismatch is explained by float normalization.
    #[must_use]
    pub const fn is_explained(&self) -> bool {
        self.unexplained.is_empty()
    }

    /// # Returns
    ///
    /// Whether an accepted pair needed the `%f` rounding rule (DV-01).
    #[must_use]
    pub fn uses_rounding(&self) -> bool {
        self.accepted.iter().any(|pair| pair.reasons.is_rounded)
    }

    /// # Returns
    ///
    /// Whether an accepted pair needed the non-finite rule (DV-02).
    #[must_use]
    pub fn uses_non_finite(&self) -> bool {
        self.accepted.iter().any(|pair| pair.reasons.is_non_finite)
    }

    /// Renders the comparison: counts, a few accepted pairs, then the unexplained difference.
    ///
    /// # Returns
    ///
    /// The rendered text.
    #[must_use]
    pub fn render(&self, max_records: usize) -> String {
        let mut lines = vec![format!(
            "expected (C++) {} records, actual (Rust) {} records; exact diff: {} missing, {} \
             unexpected; {} pairs accepted by float normalization",
            self.num_expected,
            self.num_actual,
            self.exact.num_missing(),
            self.exact.num_unexpected(),
            self.accepted.len()
        )];
        for pair in self.accepted.iter().take(max_records) {
            lines.push(format!("  ~ C++  {}", render_record(&pair.expected)));
            lines.push(format!("    Rust {}", render_record(&pair.actual)));
        }
        if !self.unexplained.is_empty() {
            lines.push(format!(
                "unexplained: {}",
                self.unexplained.render(max_records)
            ));
        }
        lines.join("\n")
    }
}

/// A C++ record and the Rust record it was paired with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedPair {
    pub expected: Vec<u8>,
    pub actual: Vec<u8>,
    pub reasons: FloatReasons,
}

/// Which float rules a pairing needed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FloatReasons {
    /// A C++ `%f` number equals the Rust float only once that float is formatted with `%f`
    /// (DV-01).
    pub is_rounded: bool,

    /// The C++ record contains `nan`/`inf` where Rust prints `null` (DV-02).
    pub is_non_finite: bool,
}

/// # Returns
///
/// Whether `token` has the shape `std::to_string(double)` prints: `-?[0-9]+\.[0-9]{6}`.
#[must_use]
pub fn is_cpp_plain_float(token: &[u8]) -> bool {
    let unsigned = token.strip_prefix(b"-").unwrap_or(token);
    let Some(dot) = unsigned.iter().position(|byte| *byte == b'.') else {
        return false;
    };
    let (integral, fraction) = (&unsigned[..dot], &unsigned[dot + 1..]);
    !integral.is_empty()
        && integral.iter().all(u8::is_ascii_digit)
        && fraction.len() == 6
        && fraction.iter().all(u8::is_ascii_digit)
}

/// Compares a C++ value with a Rust value, treating a C++ `%f` number as equal to a Rust float
/// token whose value C++ would print as exactly that text.
///
/// A Rust integer token never equals a C++ `%f` number, but a Rust float printed with exactly six
/// fraction digits (e.g. a `FormattedFloat`) is indistinguishable from the C++ `%f` output.
///
/// # Returns
///
/// Whether the values are equivalent; `reasons.is_rounded` is set if the `%f` formatting was
/// needed.
#[must_use]
pub fn is_float_equivalent(cpp: &JsonValue, rust: &JsonValue, reasons: &mut FloatReasons) -> bool {
    match (cpp, rust) {
        (JsonValue::Number(cpp_token), JsonValue::Number(rust_token)) => {
            if cpp_token == rust_token {
                return true;
            }
            if !is_cpp_plain_float(cpp_token) || !is_float_token(rust_token) {
                return false;
            }
            let Some(rust_number) = parse_finite(rust_token) else {
                return false;
            };
            // Rust's `{:.6}` and printf's `%f` both round the exact binary value (ties to even), so
            // the text comparison is exact.
            let is_equivalent = format!("{rust_number:.6}").as_bytes() == cpp_token.as_slice();
            if is_equivalent {
                reasons.is_rounded = true;
            }
            is_equivalent
        }
        (JsonValue::Array(cpp_elements), JsonValue::Array(rust_elements)) => {
            cpp_elements.len() == rust_elements.len()
                && cpp_elements
                    .iter()
                    .zip(rust_elements)
                    .all(|(cpp_element, rust_element)| {
                        is_float_equivalent(cpp_element, rust_element, reasons)
                    })
        }
        (JsonValue::Object(cpp_members), JsonValue::Object(rust_members)) => {
            if cpp_members.len() != rust_members.len() {
                return false;
            }
            let mut cpp_sorted: Vec<_> = cpp_members.iter().collect();
            let mut rust_sorted: Vec<_> = rust_members.iter().collect();
            cpp_sorted.sort_by(|left, right| left.0.cmp(&right.0));
            rust_sorted.sort_by(|left, right| left.0.cmp(&right.0));
            cpp_sorted.into_iter().zip(rust_sorted).all(
                |((cpp_key, cpp_value), (rust_key, rust_value))| {
                    cpp_key == rust_key && is_float_equivalent(cpp_value, rust_value, reasons)
                },
            )
        }
        _ => cpp == rust,
    }
}

/// An unmatched Rust record, with how many of its copies are still unpaired.
struct Candidate {
    record: Vec<u8>,
    value: JsonValue,
    remaining_count: u64,
}

/// A C++ record parsed with its non-finite tokens mapped to `null`.
struct CppRecord {
    value: JsonValue,
    has_non_finite: bool,
}

/// # Returns
///
/// The C++ canonical record (an `INVALID:` record is re-parsed from its raw text) parsed with
/// non-finite tokens as `null`, or `None` if it is not JSON even then.
fn parse_cpp_record(record: &[u8]) -> Option<CppRecord> {
    if let Some(raw) = record.strip_prefix(INVALID_PREFIX) {
        return JsonValue::parse_with_cpp_non_finite(raw).map(|value| CppRecord {
            value,
            has_non_finite: true,
        });
    }
    JsonValue::parse(record).map(|value| CppRecord {
        value,
        has_non_finite: false,
    })
}

/// # Returns
///
/// The index of the first unpaired candidate equivalent to `cpp` and the rules the pairing
/// needed, or `None` if there is none.
fn find_equivalent(cpp: &CppRecord, candidates: &[Candidate]) -> Option<(usize, FloatReasons)> {
    candidates
        .iter()
        .enumerate()
        .find_map(|(index, candidate)| {
            let mut reasons = FloatReasons {
                is_rounded: false,
                is_non_finite: cpp.has_non_finite,
            };
            (candidate.remaining_count > 0
                && is_float_equivalent(&cpp.value, &candidate.value, &mut reasons))
            .then_some((index, reasons))
        })
}

/// # Returns
///
/// The canonical serialization of `value` with every number replaced by `0`: records that can
/// be float-equivalent share it.
fn shape_key(value: &JsonValue) -> Vec<u8> {
    let mut output = Vec::new();
    write_shape(value, &mut output);
    output
}

/// Appends the shape of `value` (see [`shape_key`]).
fn write_shape(value: &JsonValue, output: &mut Vec<u8>) {
    match value {
        JsonValue::Number(_) => output.push(b'0'),
        JsonValue::Array(elements) => {
            output.push(b'[');
            for element in elements {
                write_shape(element, output);
                output.push(b',');
            }
            output.push(b']');
        }
        JsonValue::Object(members) => {
            let mut sorted: Vec<_> = members.iter().collect();
            sorted.sort_by(|left, right| left.0.cmp(&right.0));
            output.push(b'{');
            for (key, member) in sorted {
                JsonValue::String(key.clone()).write_canonical(output);
                output.push(b':');
                write_shape(member, output);
                output.push(b',');
            }
            output.push(b'}');
        }
        JsonValue::Null | JsonValue::Boolean(_) | JsonValue::String(_) => {
            value.write_canonical(output);
        }
    }
}

/// # Returns
///
/// Whether the number `token` is written as a float (with a fraction or an exponent).
fn is_float_token(token: &[u8]) -> bool {
    token.iter().any(|byte| matches!(byte, b'.' | b'e' | b'E'))
}

/// # Returns
///
/// `token` as a finite `f64`, or `None` if it is not one.
fn parse_finite(token: &[u8]) -> Option<f64> {
    std::str::from_utf8(token)
        .ok()?
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
}

#[cfg(test)]
mod tests {
    use super::FloatReasons;
    use super::RecordComparison;
    use super::is_cpp_plain_float;
    use crate::multiset::RecordMultiset;

    /// # Returns
    ///
    /// The comparison of two JSON-lines outputs.
    fn compare(cpp: &[u8], rust: &[u8]) -> RecordComparison {
        RecordComparison::compare(
            &RecordMultiset::from_jsonl(cpp),
            &RecordMultiset::from_jsonl(rust),
        )
    }

    #[test]
    fn equal_outputs_are_exact() {
        let comparison = compare(b"{\"a\":1,\"b\":2}\n", b"{\"b\":2,\"a\":1}\n");
        assert!(comparison.is_exact());
        assert!(comparison.is_explained());
        assert_eq!(comparison.accepted, vec![]);
    }

    #[test]
    fn cpp_percent_f_floats_match_the_numbers_they_round_from() {
        let comparison = compare(
            b"{\"f\":0.000000,\"g\":[1]}\n{\"f\":1.500000}\n{\"f\":-0.000000}\n{\"f\":0.500000}\n",
            b"{\"g\":[1],\"f\":1e-7}\n{\"f\":1.5}\n{\"f\":-0.0}\n{\"f\":0.5000000001}\n",
        );
        assert!(!comparison.is_exact());
        assert!(comparison.is_explained(), "{}", comparison.render(10));
        assert_eq!(comparison.accepted.len(), 4);
        assert!(comparison.uses_rounding());
        assert!(!comparison.uses_non_finite());
    }

    #[test]
    fn huge_cpp_floats_printed_in_full_match() {
        let full = format!("{{\"f\":{:.6}}}\n", 1e300_f64);
        let comparison = compare(full.as_bytes(), b"{\"f\":1e300}\n");
        assert!(comparison.is_explained(), "{}", comparison.render(10));
        assert_eq!(comparison.accepted.len(), 1);
    }

    #[test]
    fn non_rounding_differences_stay_unexplained() {
        for (cpp, rust) in [
            (
                b"{\"f\":1.500000}\n".as_slice(),
                b"{\"f\":1.6}\n".as_slice(),
            ),
            (b"{\"f\":1.5}\n", b"{\"f\":1.50}\n"),
            (b"{\"n\":1}\n", b"{\"n\":2}\n"),
            (
                b"{\"f\":1.500000,\"s\":\"a\"}\n",
                b"{\"f\":1.5,\"s\":\"b\"}\n",
            ),
            (b"{\"f\":1.500000}\n", b"{\"g\":1.5}\n"),
            (b"{\"f\":1.000000}\n", b"{\"f\":1}\n"),
            (b"{\"f\":-3.000000}\n", b"{\"f\":-3}\n"),
            (b"{\"f\":0.500000}\n", b"{\"f\":0.4999995}\n"),
            (b"{\"f\":0.000001}\n", b"{\"f\":5e-7}\n"),
        ] {
            let comparison = compare(cpp, rust);
            assert!(
                !comparison.is_explained(),
                "C++ {:?} vs Rust {:?} must not be explained",
                String::from_utf8_lossy(cpp),
                String::from_utf8_lossy(rust)
            );
            assert_eq!(comparison.accepted, vec![]);
        }
    }

    #[test]
    fn cpp_non_finite_tokens_match_rust_null() {
        let comparison = compare(b"{\"f\":nan,\"g\":-inf}\n", b"{\"f\":null,\"g\":null}\n");
        assert!(comparison.is_explained(), "{}", comparison.render(10));
        assert_eq!(
            comparison.accepted[0].reasons,
            FloatReasons {
                is_rounded: false,
                is_non_finite: true,
            }
        );
        assert!(comparison.uses_non_finite());
    }

    #[test]
    fn each_rust_record_explains_one_cpp_record() {
        let comparison = compare(b"{\"f\":1.500000}\n{\"f\":1.500000}\n", b"{\"f\":1.5}\n");
        assert_eq!(comparison.accepted.len(), 1);
        assert_eq!(
            comparison.unexplained.missing,
            vec![(b"{\"f\":1.500000}".to_vec(), 1)]
        );
        assert_eq!(comparison.unexplained.unexpected, vec![]);
        let rendered = comparison.render(10);
        assert!(rendered.contains("unexplained: 1 missing"), "{rendered}");
    }

    #[test]
    fn plain_float_shape_is_six_fraction_digits() {
        for token in ["1.500000", "-0.000000", "123456.789012"] {
            assert!(is_cpp_plain_float(token.as_bytes()), "{token}");
        }
        for token in [
            "1.5",
            "1.5000000",
            "1e10",
            ".500000",
            "-.500000",
            "1.50000a",
            "1",
        ] {
            assert!(!is_cpp_plain_float(token.as_bytes()), "{token}");
        }
    }
}
