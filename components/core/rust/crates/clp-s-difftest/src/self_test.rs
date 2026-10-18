//! The harness self-test: the C++ `clp-s` run as the reader under test must reproduce its own
//! oracles, so every verdict is predictable from the corpus.

use crate::corpus::Corpus;
use crate::corpus::Expectation;
use crate::harness::CaseReport;
use crate::harness::Failure;
use crate::harness::Verdict;

/// Checks a report of the C++ reader against its own oracles.
///
/// Untagged cases pass, rejection fixtures are read (C++ supports them), and cases declaring a
/// divergence fail because C++ cannot reproduce the Rust expectation.
///
/// # Returns
///
/// Whether the verdict is the one the C++ reader must get.
#[must_use]
pub fn is_consistent_cpp_verdict(corpus: &Corpus, report: &CaseReport) -> bool {
    if let Some(oracle) = corpus
        .archive_oracles
        .iter()
        .find(|oracle| oracle.archive_name() == report.case)
    {
        return match (&report.verdict, oracle.expectation) {
            (Verdict::Pass, Expectation::Readable) => true,
            (Verdict::Fail(Failure::NotRejected { num_records }), Expectation::Reject) => {
                *num_records == oracle.num_records
            }
            _ => false,
        };
    }
    let is_divergent = corpus
        .queries
        .iter()
        .find(|case| case.id == report.case)
        .is_some_and(crate::corpus::QueryCase::is_divergent);
    match &report.verdict {
        Verdict::Pass => !is_divergent,
        Verdict::Skipped { .. } => true,
        Verdict::Fail(
            Failure::DivergenceNotReproduced { .. } | Failure::ExpectationMismatch { .. },
        ) => is_divergent,
        Verdict::Accepted { .. } | Verdict::Fail(_) => false,
    }
}
