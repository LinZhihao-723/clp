//! The reference-evaluator hook: `clp_s_query`'s evaluator over the C++-loaded fixture records
//! must reproduce the ids `scripts/reference_eval.py` recorded for every fixture query and the
//! counts of the count-only divergent cases; the C++ search records the harness compares by id
//! must equal the fixture's `x` records.

use std::collections::HashMap;

use clp_s_difftest::canonical::canonical_record;
use clp_s_difftest::corpus::Corpus;
use clp_s_difftest::corpus::OutputKind;
use clp_s_difftest::corpus::QueryTarget;
use clp_s_difftest::corpus::RecordKey;
use clp_s_difftest::corpus::prototype_root;
use clp_s_difftest::cpp::CppClpS;
use clp_s_difftest::harness::records_by_id;
use clp_s_difftest::reference::ReferenceRecords;

#[test]
fn the_rust_reference_evaluator_reproduces_reference_eval_py() -> anyhow::Result<()> {
    let corpus = Corpus::load(&prototype_root())?;
    let cpp = CppClpS::default();
    let work = tempfile::tempdir()?;
    let mut records: HashMap<String, ReferenceRecords> = HashMap::new();
    let mut disagreements = Vec::new();
    let mut num_checked = 0;
    for case in &corpus.queries {
        let QueryTarget::Fixture(name) = &case.target else {
            continue;
        };
        let Some(expected) = corpus
            .query_oracle(&case.id, OutputKind::Stdout)
            .and_then(|oracle| oracle.reference_ids.clone())
        else {
            continue;
        };
        if !records.contains_key(name) {
            let fixture = corpus.fixture(name).expect("the query targets a fixture");
            let loaded = ReferenceRecords::load(&cpp, fixture, work.path())?;
            records.insert(name.clone(), loaded);
        }
        let mut expected = expected;
        expected.sort();
        let actual = match records[name].matching_ids(case, case.id_key()) {
            Ok(actual) => actual,
            Err(error) => {
                disagreements.push(format!("{}: {error}", case.id));
                continue;
            }
        };
        if actual != expected {
            disagreements.push(format!(
                "{}: expected {expected:?}, got {actual:?}",
                case.id
            ));
        }
        num_checked += 1;
    }
    assert!(num_checked > 200, "only {num_checked} cases were checked");
    assert_eq!(disagreements, Vec::<String>::new());
    Ok(())
}

#[test]
fn count_only_cases_match_the_rust_reference_evaluator() -> anyhow::Result<()> {
    let corpus = Corpus::load(&prototype_root())?;
    let cpp = CppClpS::default();
    let work = tempfile::tempdir()?;
    let mut disagreements = Vec::new();
    let mut num_checked = 0;
    for case in &corpus.queries {
        let (QueryTarget::Fixture(name), None, Some(expected_count)) = (
            &case.target,
            &case.expected_rust_ids,
            case.expected_rust_count,
        ) else {
            continue;
        };
        let fixture = corpus.fixture(name).expect("the query targets a fixture");
        let records = ReferenceRecords::load(&cpp, fixture, work.path())?;
        let num_matching = records.matching_records(case)?.len() as u64;
        if num_matching != expected_count {
            disagreements.push(format!(
                "{}: expected {expected_count}, the reference evaluator matches {num_matching}",
                case.id
            ));
        }
        num_checked += 1;
    }
    assert!(num_checked >= 3, "only {num_checked} cases were checked");
    assert_eq!(disagreements, Vec::<String>::new());
    Ok(())
}

#[test]
fn search_records_equal_their_decompressed_records() -> anyhow::Result<()> {
    let corpus = Corpus::load(&prototype_root())?;
    let mut disagreements = Vec::new();
    let mut num_checked = 0;
    for case in &corpus.queries {
        let QueryTarget::Fixture(name) = &case.target else {
            continue;
        };
        if !case.options.projection.is_empty() {
            continue;
        }
        let Some(oracle) = corpus.query_oracle(&case.id, OutputKind::Stdout) else {
            continue;
        };
        let id_key = case.id_key();
        let mut decompressed: HashMap<RecordKey, Vec<Vec<u8>>> = HashMap::new();
        for archive in corpus
            .archive_oracles
            .iter()
            .filter(|archive| archive.fixture == *name)
        {
            let path = archive.oracle.as_ref().expect("a readable fixture");
            for (id, record) in records_by_id(&std::fs::read(path)?, id_key) {
                decompressed
                    .entry(id)
                    .or_default()
                    .push(canonical_record(&record));
            }
        }
        for (id, record) in records_by_id(&std::fs::read(&oracle.file)?, id_key) {
            let is_found = decompressed
                .get(&id)
                .is_some_and(|candidates| candidates.contains(&canonical_record(&record)));
            if !is_found {
                disagreements.push(format!("{}: {id}", case.id));
            }
            num_checked += 1;
        }
    }
    assert!(
        num_checked > 1000,
        "only {num_checked} records were checked"
    );
    assert_eq!(disagreements, Vec::<String>::new());
    Ok(())
}
