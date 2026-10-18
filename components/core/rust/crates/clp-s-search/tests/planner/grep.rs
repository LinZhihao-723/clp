//! The `ClpString` prefilter against the glob over every stored `ClpString` value of the
//! fixtures, with patterns derived from those values: the prefilter must never reject a value the
//! pattern matches, and a value it accepts without a glob check must match.

use clp_s_query::PatternPiece;
use clp_s_query::TextPattern;
use clp_s_search::EncodedTextMatch;

use crate::support::ALL_DICTIONARIES;
use crate::support::ArchiveRows;
use crate::support::OpenedArchive;
use crate::support::StoredValue;
use crate::support::fixture;

/// A deterministic xorshift generator.
struct Random(u64);

impl Random {
    /// # Returns
    ///
    /// The next pseudo-random value.
    const fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// # Returns
    ///
    /// A pseudo-random value below `bound` (or 0 if `bound` is 0).
    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound.max(1)).expect("bounds fit u64"))
            .expect("the value is below a usize bound")
    }
}

/// # Returns
///
/// A pattern derived from `value`: a window of it, with some bytes replaced by `?` or runs by
/// `*`, and optional `*` at either end.
fn derive_pattern(value: &[u8], random: &mut Random) -> TextPattern {
    const MAX_WINDOW_SIZE: usize = 32;

    let begin = random.below(value.len());
    let end = begin + random.below((value.len() - begin).min(MAX_WINDOW_SIZE) + 1);
    let mut pieces = Vec::new();
    if 0 != begin || 0 == random.below(3) {
        pieces.push(PatternPiece::AnyRun);
    }
    let mut position = begin;
    while position < end {
        match random.below(16) {
            0 => {
                pieces.push(PatternPiece::AnyByte);
                position += 1;
            }
            1 => {
                pieces.push(PatternPiece::AnyRun);
                position += 1 + random.below(4);
            }
            _ => {
                pieces.push(PatternPiece::Literal(
                    vec![value[position]].into_boxed_slice(),
                ));
                position += 1;
            }
        }
    }
    if end != value.len() || 0 == random.below(3) {
        pieces.push(PatternPiece::AnyRun);
    }
    TextPattern::new(pieces)
}

/// Counts over the whole fuzz.
#[derive(Default)]
struct Tally {
    checks: u64,
    glob_matches: u64,
    exact_prefilter_matches: u64,
    violations: Vec<String>,
}

/// One stored `ClpString` value: its logtype id, encoded variables and decoded text.
type EncodedValue = (u64, Vec<i64>, Vec<u8>);

/// Checks the prefilter of `pattern` against every value of an archive.
fn check_pattern(
    name: &str,
    pattern: &TextPattern,
    is_case_sensitive: bool,
    prefilter: &EncodedTextMatch,
    values: &[EncodedValue],
    tally: &mut Tally,
) {
    for (logtype, encoded_variables, text) in values {
        tally.checks += 1;
        let is_glob_match = pattern.matches(text, is_case_sensitive);
        tally.glob_matches += u64::from(is_glob_match);
        let (is_accepted, is_exact) = match prefilter {
            EncodedTextMatch::Never => (false, true),
            EncodedTextMatch::DecodeAll => (true, false),
            EncodedTextMatch::Subqueries(subqueries) => {
                let matching: Vec<_> = subqueries
                    .iter()
                    .filter(|subquery| {
                        subquery.logtypes.contains(*logtype)
                            && matches_in_order(&subquery.variables, encoded_variables)
                    })
                    .collect();
                (
                    !matching.is_empty(),
                    matching
                        .iter()
                        .any(|subquery| !subquery.is_glob_check_required),
                )
            }
        };
        if is_glob_match && !is_accepted {
            tally.violations.push(format!(
                "{name}: {pattern:?} (case-sensitive: {is_case_sensitive}) misses {:?}",
                String::from_utf8_lossy(text)
            ));
        }
        if is_accepted && is_exact {
            tally.exact_prefilter_matches += 1;
            if !is_glob_match {
                tally.violations.push(format!(
                    "{name}: {pattern:?} (case-sensitive: {is_case_sensitive}) claims {:?} \
                     without a glob check",
                    String::from_utf8_lossy(text)
                ));
            }
        }
    }
}

#[test]
fn prefilters_never_miss_and_never_claim_false_matches() -> anyhow::Result<()> {
    const NUM_PATTERNS_PER_ARCHIVE: usize = 400;

    let mut random = Random(0x9e37_79b9_7f4a_7c15);
    let mut tally = Tally::default();
    for name in [
        "logs.ts",
        "search.ts",
        "strings.default",
        "long-strings.default",
        "keys.default",
    ] {
        let path = fixture(name);
        let rows = ArchiveRows::load(&path)?;
        let archive = OpenedArchive::open(&path)?;
        archive.load(ALL_DICTIONARIES)?;
        let dictionaries = archive.storage.dictionaries();
        let logtypes = dictionaries
            .logtype
            .get()
            .expect("the logtype dictionary is loaded");
        let variables = dictionaries
            .variable
            .get()
            .expect("the variable dictionary is loaded");
        let values: Vec<EncodedValue> = rows
            .tables
            .iter()
            .flatten()
            .flat_map(|row| row.columns.iter().flatten())
            .filter_map(|stored| match stored {
                StoredValue::Encoded {
                    logtype,
                    variables,
                    text,
                } => Some((*logtype, variables.clone(), text.clone())),
                _ => None,
            })
            .collect();
        assert!(!values.is_empty(), "{name} has ClpString values");

        for index in 0..NUM_PATTERNS_PER_ARCHIVE {
            let source = &values[random.below(values.len())].2;
            let pattern = if 0 == index % 10 {
                TextPattern::new(vec![PatternPiece::Literal(
                    source.clone().into_boxed_slice(),
                )])
            } else {
                derive_pattern(source, &mut random)
            };
            let is_case_sensitive = 0 != index % 4;
            let prefilter = EncodedTextMatch::new(&pattern, is_case_sensitive, logtypes, variables);
            check_pattern(
                name,
                &pattern,
                is_case_sensitive,
                &prefilter,
                &values,
                &mut tally,
            );
        }
    }
    println!(
        "{} prefilter checks, {} glob matches, {} exact prefilter matches",
        tally.checks, tally.glob_matches, tally.exact_prefilter_matches
    );
    assert!(
        tally.glob_matches > 1_000,
        "the patterns match often enough to test misses"
    );
    assert!(
        tally.exact_prefilter_matches > 100,
        "exact prefilter matches are exercised"
    );
    assert_eq!(
        tally.violations.iter().take(20).collect::<Vec<_>>(),
        Vec::<&String>::new()
    );
    Ok(())
}

/// # Returns
///
/// Whether `variables` passes `tests` in order (C++ `SubQuery::matches_vars`).
fn matches_in_order(tests: &[clp_s_search::EncodedVariableTest], variables: &[i64]) -> bool {
    let mut tests = tests.iter().peekable();
    for variable in variables {
        if let Some(test) = tests.peek()
            && test.matches(*variable)
        {
            tests.next();
        }
    }
    tests.peek().is_none()
}
