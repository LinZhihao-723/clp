//! Dataset equivalence (`docs/DESIGN.md` §11.4): the Rust library decompresses the bench, spider
//! and synthetic archives to the C++ `x` output (canonical multiset), and answers every dataset
//! query of the suite like C++ (up to the declared divergences) and like `clp_s_query`'s reference
//! evaluator over the `x` records. The archives are large, so the tests are opt-in: `cargo test
//! -p clp-s-difftest --release --test datasets -- --ignored`.

use clp_s_difftest::corpus::Corpus;
use clp_s_difftest::corpus::QueryTarget;
use clp_s_difftest::corpus::prototype_root;
use clp_s_difftest::cpp::CppClpS;
use clp_s_difftest::harness::Harness;
use clp_s_difftest::harness::HarnessOptions;
use clp_s_difftest::harness::Summary;
use clp_s_difftest::harness::Verdict;
use clp_s_difftest::registry::Registry;
use clp_s_difftest::rust::RustLibraryReader;
use clp_s_reader::JsonOptions;

#[test]
#[ignore = "decompresses the multi-million-record dataset archives"]
fn rust_library_reproduces_every_dataset_x_oracle() -> anyhow::Result<()> {
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
        HarnessOptions::default(),
    );
    let reports: Vec<_> = harness
        .run_datasets()
        .into_iter()
        .filter(|report| report.case.starts_with("x:"))
        .collect();
    let cases: Vec<&str> = reports.iter().map(|report| report.case.as_str()).collect();
    assert_eq!(cases, vec!["x:bench", "x:spider", "x:synthetic"]);
    let not_passed: Vec<String> = reports
        .iter()
        .filter(|report| Verdict::Pass != report.verdict)
        .map(ToString::to_string)
        .collect();
    assert_eq!(not_passed, Vec::<String>::new());
    Ok(())
}

#[test]
#[ignore = "searches the multi-million-record dataset archives"]
fn rust_library_answers_every_dataset_query_like_cpp_and_the_reference() -> anyhow::Result<()> {
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
            case_filter: corpus
                .queries
                .iter()
                .filter(|case| matches!(case.target, QueryTarget::Dataset(_)))
                .map(|case| case.id.clone())
                .collect(),
            ..HarnessOptions::default()
        },
    );
    let reports = harness.run_datasets();
    let not_passed: Vec<String> = reports
        .iter()
        .filter(|report| report.verdict.is_failure())
        .map(ToString::to_string)
        .collect();
    assert_eq!(not_passed, Vec::<String>::new());
    assert_eq!(
        Summary::of(&reports),
        Summary {
            num_passed: 30,
            num_accepted: 2,
            num_skipped: 0,
            num_failed: 0,
        }
    );
    Ok(())
}
