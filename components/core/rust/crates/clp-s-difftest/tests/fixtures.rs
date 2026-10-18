//! Fixture equivalence (`docs/design.md` §11.2): the C++ `clp-s` as the reader under test (the
//! harness self-test) must reproduce every fixture `x` oracle byte-for-byte in canonical form and
//! read the rejection fixtures (the rejection is the Rust reader's scope, not the format's); the
//! Rust library must reproduce every oracle up to registered divergences and reject the rejection
//! fixtures with `UnsupportedArchive` at open.

use clp_s_difftest::canonical::CanonicalDigest;
use clp_s_difftest::corpus::Corpus;
use clp_s_difftest::corpus::prototype_root;
use clp_s_difftest::cpp::CppClpS;
use clp_s_difftest::cpp::CppReader;
use clp_s_difftest::harness::Failure;
use clp_s_difftest::harness::Harness;
use clp_s_difftest::harness::HarnessOptions;
use clp_s_difftest::harness::Verdict;
use clp_s_difftest::registry::DivergenceId;
use clp_s_difftest::registry::Registry;
use clp_s_difftest::rust::RustLibraryReader;
use clp_s_difftest::self_test::is_consistent_cpp_verdict;
use clp_s_reader::JsonOptions;

#[test]
fn cpp_reproduces_every_fixture_oracle() -> anyhow::Result<()> {
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
    let reports = harness.run_fixtures();
    assert_eq!(reports.len(), corpus.archive_oracles.len());
    let inconsistent: Vec<String> = reports
        .iter()
        .filter(|report| !is_consistent_cpp_verdict(&corpus, report))
        .map(ToString::to_string)
        .collect();
    assert_eq!(inconsistent, Vec::<String>::new());
    let num_passed = reports
        .iter()
        .filter(|report| report.verdict == Verdict::Pass)
        .count();
    let num_not_rejected = reports
        .iter()
        .filter(|report| matches!(report.verdict, Verdict::Fail(Failure::NotRejected { .. })))
        .count();
    assert_eq!((num_passed, num_not_rejected), (57, 2));
    Ok(())
}

#[test]
fn canonical_digests_equal_corpus_py_for_every_oracle() -> anyhow::Result<()> {
    let corpus = Corpus::load(&prototype_root())?;
    let mut num_checked = 0;
    for oracle in &corpus.archive_oracles {
        let (Some(path), Some(expected)) = (&oracle.oracle, &oracle.canonical_sha256) else {
            continue;
        };
        let digest = CanonicalDigest::of_jsonl(&std::fs::read(path)?);
        assert_eq!(&digest.sha256, expected, "{}", path.display());
        assert_eq!(digest.count, oracle.num_records, "{}", path.display());
        num_checked += 1;
    }
    for oracle in &corpus.query_oracles {
        let Some(expected) = &oracle.canonical_sha256 else {
            continue;
        };
        if oracle.file.starts_with(&corpus.data_root) {
            continue;
        }
        let digest = CanonicalDigest::of_jsonl(&std::fs::read(&oracle.file)?);
        assert_eq!(&digest.sha256, expected, "{}", oracle.file.display());
        assert_eq!(digest.count, oracle.count, "{}", oracle.file.display());
        num_checked += 1;
    }
    assert_eq!(num_checked, 57 + 303);
    Ok(())
}

#[test]
fn rust_library_reproduces_every_fixture_oracle() -> anyhow::Result<()> {
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
    let reports = harness.run_fixtures();
    assert_eq!(reports.len(), corpus.archive_oracles.len());
    let failures: Vec<String> = reports
        .iter()
        .filter(|report| report.verdict.is_failure())
        .map(ToString::to_string)
        .collect();
    assert_eq!(failures, Vec::<String>::new());
    let mut accepted: Vec<(String, Vec<&str>)> = reports
        .iter()
        .filter_map(|report| match &report.verdict {
            Verdict::Accepted { divergences, .. } => Some((
                report.case.clone(),
                divergences.iter().map(DivergenceId::as_str).collect(),
            )),
            _ => None,
        })
        .collect();
    accepted.sort();
    assert_eq!(
        accepted,
        vec![
            ("arrays.structurize-arrays.clps".to_owned(), vec!["SX-02"]),
            ("floats.nofloat.clps".to_owned(), vec!["DV-01"]),
            ("polymorphic.nofloat.clps".to_owned(), vec!["DV-01"]),
            ("scalars.mfa".to_owned(), vec!["SX-01"]),
            ("scalars.nofloat.clps".to_owned(), vec!["DV-01"]),
            ("search.ts-nofloat.clps".to_owned(), vec!["DV-01"]),
            ("timestamps-float.ts-nofloat.clps".to_owned(), vec!["DV-01"]),
        ]
    );
    Ok(())
}

#[test]
fn rust_library_emulating_cpp_floats_matches_every_oracle_exactly() -> anyhow::Result<()> {
    let root = prototype_root();
    let corpus = Corpus::load(&root)?;
    let registry = Registry::load(&root)?;
    let cpp = CppClpS::default();
    let reader = RustLibraryReader::new(JsonOptions {
        should_emulate_cpp_plain_floats: true,
    })?;
    let work = tempfile::tempdir()?;
    let harness = Harness::new(
        &corpus,
        &registry,
        &cpp,
        &reader,
        work.path(),
        HarnessOptions::default(),
    );
    let not_passed: Vec<String> = harness
        .run_fixtures()
        .iter()
        .filter(|report| Verdict::Pass != report.verdict)
        .map(|report| report.case.clone())
        .collect();
    assert_eq!(
        not_passed,
        vec!["scalars.mfa", "arrays.structurize-arrays.clps"]
    );
    Ok(())
}
