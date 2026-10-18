//! The archive-dependence classifier of design report §8.3.
//!
//! For an undeclared mismatching record, C++ is re-run on an archive of that record alone. If C++
//! on the single record agrees with Rust while C++ on the full archive does not, the mismatch is
//! archive dependent, and the flipping predicate suggests a registry entry. Suggestions are hints
//! for triage, never acceptances.

use std::path::Path;

use crate::canonical::INVALID_PREFIX;
use crate::corpus::Fixture;
use crate::corpus::QueryCase;
use crate::cpp::CppClpS;
use crate::cpp::CppOutput;
use crate::multiset::MultisetDiff;
use crate::registry::ArchiveDependenceRule;
use crate::registry::DivergenceId;
use crate::registry::Registry;

/// The outcome of probing one mismatching record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeFinding {
    /// The canonical record.
    pub record: Vec<u8>,

    /// Whether the Rust reader returned the record.
    pub is_rust_match: bool,

    /// Whether C++ returns the record from an archive holding only it.
    pub is_cpp_single_match: bool,

    /// The suggested entry, if the mismatch is archive dependent and a rule fits.
    pub suggestion: Option<DivergenceId>,
}

impl ProbeFinding {
    /// # Returns
    ///
    /// Whether C++ on the single record agrees with Rust (C++ on the full archive does not, or
    /// the record would not have been probed).
    #[must_use]
    pub const fn is_archive_dependent(&self) -> bool {
        self.is_rust_match == self.is_cpp_single_match
    }
}

/// Probes up to `max_records` mismatching records of a fixture query case.
///
/// # Returns
///
/// One finding per probed record on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if a probe input cannot be written.
/// * Forwards [`CppClpS::compress`]'s return values on failure.
/// * Forwards [`CppClpS::search`]'s return values on failure.
pub fn probe_archive_dependence(
    cpp: &CppClpS,
    registry: &Registry,
    fixture: &Fixture,
    case: &QueryCase,
    diff: &MultisetDiff,
    work_dir: &Path,
    max_records: usize,
) -> anyhow::Result<Vec<ProbeFinding>> {
    let candidates = diff
        .missing
        .iter()
        .map(|(record, _)| (record, false))
        .chain(diff.unexpected.iter().map(|(record, _)| (record, true)))
        .filter(|(record, _)| !record.starts_with(INVALID_PREFIX))
        .take(max_records);
    let mut findings = Vec::new();
    for (index, (record, is_rust_match)) in candidates.enumerate() {
        let probe_dir = work_dir.join(format!("probe-{index}"));
        std::fs::create_dir_all(&probe_dir)?;
        let input = probe_dir.join("record.jsonl");
        std::fs::write(&input, [record.as_slice(), b"\n"].concat())?;
        let archives = cpp.compress(&input, &probe_dir.join("archives"), &fixture.flags)?;
        let mut is_cpp_single_match = false;
        for archive in &archives {
            let run = cpp.search(archive, &case.kql, &case.options, &CppOutput::Stdout)?;
            is_cpp_single_match |=
                run.exit_code == 0 && run.output.iter().any(|byte| *byte != b'\n');
        }
        let mut finding = ProbeFinding {
            record: record.clone(),
            is_rust_match,
            is_cpp_single_match,
            suggestion: None,
        };
        if finding.is_archive_dependent() {
            finding.suggestion = suggest(registry, &case.kql, record);
        }
        findings.push(finding);
    }
    Ok(findings)
}

/// Suggests the archive-dependence entry whose flipping predicate occurs in `kql`.
///
/// This is a lexical approximation of design report §8.3's rules; the order-dependent rules need
/// pair runs and are never suggested.
///
/// # Returns
///
/// The suggested entry, if a rule fits.
#[must_use]
pub fn suggest(registry: &Registry, kql: &str, record: &[u8]) -> Option<DivergenceId> {
    let terms = Term::parse_all(kql);
    let has_empty_key = record.windows(3).any(|window| window == b"\"\":");
    let has_array = record.contains(&b'[');
    let rule =
        if terms.iter().any(|term| {
            term.is_negated
                && term.column.as_deref().is_some_and(|column| column != "*")
                && term.value == "*"
        }) {
            ArchiveDependenceRule::NotExistsPath
        } else if terms.iter().any(|term| {
            term.column.as_deref() == Some("*") && term.value.eq_ignore_ascii_case("null")
        }) {
            ArchiveDependenceRule::WildcardNull
        } else if terms.iter().any(|term| {
            term.is_negated && term.column.as_deref().is_none_or(|column| column == "*")
        }) {
            ArchiveDependenceRule::NegatedWildcardOrBareValue
        } else if has_empty_key && terms.iter().any(Term::has_wildcard_column) {
            ArchiveDependenceRule::WildcardEmptyKey
        } else if has_array && terms.iter().any(Term::has_partial_wildcard_column) {
            ArchiveDependenceRule::PartialWildcardOverArray
        } else {
            return None;
        };
    registry
        .with_rule(rule)
        .next()
        .map(|entry| entry.id.clone())
}

/// One `column: value` or bare-value term of a KQL text, split on whitespace and parentheses.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Term {
    column: Option<String>,
    value: String,
    is_negated: bool,
}

impl Term {
    /// # Returns
    ///
    /// The terms of `kql`, in order.
    fn parse_all(kql: &str) -> Vec<Self> {
        let words: Vec<&str> = kql
            .split(|character: char| character.is_whitespace() || matches!(character, '(' | ')'))
            .filter(|word| !word.is_empty())
            .collect();
        let mut terms = Vec::new();
        let mut index = 0;
        while index < words.len() {
            let word = words[index];
            let is_negated = index > 0 && words[index - 1].eq_ignore_ascii_case("NOT");
            index += 1;
            if ["AND", "OR", "NOT"]
                .iter()
                .any(|keyword| word.eq_ignore_ascii_case(keyword))
            {
                continue;
            }
            let Some(colon) = word.find(':') else {
                terms.push(Self {
                    column: None,
                    value: word.to_owned(),
                    is_negated,
                });
                continue;
            };
            let mut value = word[colon + 1..].to_owned();
            if value.is_empty() && index < words.len() {
                words[index].clone_into(&mut value);
                index += 1;
            }
            terms.push(Self {
                column: Some(word[..colon].to_owned()),
                value,
                is_negated,
            });
        }
        terms
    }

    /// # Returns
    ///
    /// Whether the column contains a wildcard.
    fn has_wildcard_column(&self) -> bool {
        self.column
            .as_deref()
            .is_some_and(|column| column.contains('*'))
    }

    /// # Returns
    ///
    /// Whether the column is a partial wildcard (a wildcard plus other tokens).
    fn has_partial_wildcard_column(&self) -> bool {
        self.column
            .as_deref()
            .is_some_and(|column| column.contains('*') && column != "*")
    }
}

#[cfg(test)]
mod tests {
    use super::Term;
    use super::probe_archive_dependence;
    use super::suggest;
    use crate::corpus::Corpus;
    use crate::corpus::prototype_root;
    use crate::cpp::CppClpS;
    use crate::multiset::MultisetDiff;
    use crate::registry::DivergenceId;
    use crate::registry::Registry;

    #[test]
    fn terms_split_columns_values_and_negation() {
        assert_eq!(
            Term::parse_all("NOT a.b: * AND (c:1 OR bare)"),
            vec![
                Term {
                    column: Some("a.b".to_owned()),
                    value: "*".to_owned(),
                    is_negated: true,
                },
                Term {
                    column: Some("c".to_owned()),
                    value: "1".to_owned(),
                    is_negated: false,
                },
                Term {
                    column: None,
                    value: "bare".to_owned(),
                    is_negated: false,
                },
            ]
        );
    }

    #[test]
    fn suggestions_follow_the_flipping_predicate() -> anyhow::Result<()> {
        let registry = Registry::load(&prototype_root())?;
        let suggestion = |kql: &str, record: &[u8]| {
            suggest(&registry, kql, record).map(|id| id.as_str().to_owned())
        };
        assert_eq!(suggestion("NOT a.b: *", b"{}").as_deref(), Some("DV-07"));
        assert_eq!(suggestion("*: null", b"{}").as_deref(), Some("DV-09"));
        assert_eq!(suggestion("NOT ERROR", b"{}").as_deref(), Some("DV-05"));
        assert_eq!(suggestion("NOT *: x", b"{}").as_deref(), Some("DV-05"));
        assert_eq!(
            suggestion("a.*: 1", br#"{"":{"a":1}}"#).as_deref(),
            Some("DV-38")
        );
        assert_eq!(
            suggestion("a.*: 1", br#"{"a":[{"b":1}]}"#).as_deref(),
            Some("DV-36")
        );
        assert_eq!(suggestion("a.*: 1", br#"{"a":{"b":1}}"#), None);
        assert_eq!(suggestion("a: 1", br#"{"a":[1]}"#), None);
        Ok(())
    }

    #[test]
    fn probing_reruns_cpp_on_the_single_record() -> anyhow::Result<()> {
        let root = prototype_root();
        let corpus = Corpus::load(&root)?;
        let registry = Registry::load(&root)?;
        let fixture = corpus
            .fixture("search.ts")
            .expect("the corpus has search.ts");
        let mut case = corpus.queries[0].clone();
        case.kql = "NOT nothere: *".to_owned();
        let record = br#"{"level":"INFO","rid":"rx","ts":1700000000000}"#.to_vec();
        let work = tempfile::tempdir()?;
        let findings = probe_archive_dependence(
            &CppClpS::default(),
            &registry,
            fixture,
            &case,
            &MultisetDiff {
                missing: vec![(record.clone(), 1)],
                unexpected: vec![(b"INVALID:x".to_vec(), 1)],
            },
            work.path(),
            5,
        )?;
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].record, record);
        assert!(!findings[0].is_rust_match);
        assert!(!findings[0].is_cpp_single_match);
        assert!(findings[0].is_archive_dependent());
        assert_eq!(
            findings[0].suggestion.as_ref().map(DivergenceId::as_str),
            Some("DV-07")
        );
        Ok(())
    }
}
