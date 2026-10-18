//! Query-suite equivalence (`docs/DESIGN.md` §11.3): the harness self-test (the C++ `clp-s` as the
//! reader under test must reproduce the oracle of every fixture case that declares no divergence,
//! and must fail every case that declares one), and the Rust library against the three oracles
//! (C++, the declared expectations, and `clp_s_query`'s reference evaluator).

use clp_s_difftest::corpus::Corpus;
use clp_s_difftest::corpus::prototype_root;
use clp_s_difftest::cpp::CppClpS;
use clp_s_difftest::cpp::CppReader;
use clp_s_difftest::harness::Harness;
use clp_s_difftest::harness::HarnessOptions;
use clp_s_difftest::harness::Summary;
use clp_s_difftest::harness::Verdict;
use clp_s_difftest::registry::Registry;
use clp_s_difftest::rust::RustLibraryReader;
use clp_s_difftest::self_test::is_consistent_cpp_verdict;
use clp_s_reader::JsonOptions;

#[test]
fn cpp_reproduces_every_fixture_query_oracle() -> anyhow::Result<()> {
    let root = prototype_root();
    let corpus = Corpus::load(&root)?;
    let registry = Registry::load(&root)?;
    let cpp = CppClpS::default();
    let reader = CppReader::new(cpp.clone());
    let work = tempfile::tempdir()?;
    let harness = Harness::new(
        &corpus,
        &registry,
        &cpp,
        &reader,
        work.path(),
        HarnessOptions::default(),
    );
    let reports = harness.run_queries();
    let inconsistent: Vec<String> = reports
        .iter()
        .filter(|report| !is_consistent_cpp_verdict(&corpus, report))
        .map(ToString::to_string)
        .collect();
    assert_eq!(inconsistent, Vec::<String>::new());
    let summary = Summary::of(&reports);
    assert_eq!(
        summary,
        Summary {
            num_passed: 232,
            num_accepted: 0,
            num_skipped: 3,
            num_failed: 75,
        },
        "{summary}"
    );
    Ok(())
}

#[test]
fn rust_library_reproduces_every_fixture_query_up_to_registered_divergences() -> anyhow::Result<()>
{
    let root = prototype_root();
    let corpus = Corpus::load(&root)?;
    let registry = Registry::load(&root)?;
    let cpp = CppClpS::default();
    let reader = RustLibraryReader::new(JsonOptions::default())?;
    let work = tempfile::tempdir()?;
    let harness = Harness::new(
        &corpus,
        &registry,
        &cpp,
        &reader,
        work.path(),
        HarnessOptions {
            is_reference_evaluator_enabled: true,
            ..HarnessOptions::default()
        },
    );
    let reports = harness.run_queries();
    let failures: Vec<String> = reports
        .iter()
        .filter(|report| report.verdict.is_failure())
        .map(|report| {
            let output = report
                .output
                .map(|output| format!(" [{output}]"))
                .unwrap_or_default();
            format!("{}{output}", report.case)
        })
        .collect();
    assert_eq!(failures, Vec::<String>::new(), "{reports:#?}");
    let num_skipped_non_stdout = reports
        .iter()
        .filter(|report| matches!(report.verdict, Verdict::Skipped { .. }))
        .count();
    assert_eq!(
        Summary::of(&reports),
        Summary {
            num_passed: 229,
            num_accepted: 78,
            num_skipped: num_skipped_non_stdout,
            num_failed: 0,
        }
    );
    assert_eq!(
        num_skipped_non_stdout, 3,
        "only results-cache cases are skipped"
    );
    Ok(())
}
