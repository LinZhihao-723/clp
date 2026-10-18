//! `clp-s-rs search` as a process: query-suite equivalence through the differential harness, the
//! exit codes, sessions over directories, and the options C++ shares.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

use clp_s_difftest::corpus::Corpus;
use clp_s_difftest::corpus::prototype_root;
use clp_s_difftest::cpp::CppClpS;
use clp_s_difftest::harness::Harness;
use clp_s_difftest::harness::HarnessOptions;
use clp_s_difftest::registry::Registry;
use clp_s_difftest::rust::RustCliReader;

/// The `clp-s-rs` binary under test.
const BINARY: &str = env!("CARGO_BIN_EXE_clp-s-rs");

/// # Returns
///
/// The path of fixture archive `name`.
fn fixture(name: &str) -> PathBuf {
    prototype_root()
        .join("tests/fixtures/archives")
        .join(format!("{name}.clps"))
}

/// # Returns
///
/// The non-empty lines of `jsonl`, sorted.
fn sorted_lines(jsonl: &[u8]) -> Vec<Vec<u8>> {
    let mut lines: Vec<Vec<u8>> = jsonl
        .split(|byte| b'\n' == *byte)
        .filter(|line| !line.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    lines.sort();
    lines
}

/// # Returns
///
/// The sorted lines of the C++ stdout oracle of suite case `id`.
fn query_oracle_lines(id: &str) -> Vec<Vec<u8>> {
    sorted_lines(
        &std::fs::read(
            prototype_root()
                .join("tests/queries/oracles")
                .join(format!("{id}.jsonl")),
        )
        .expect("the query oracle is readable"),
    )
}

/// # Returns
///
/// The output of `clp-s-rs search <archives...> <args...>`.
fn search(archives: &[&Path], args: &[&str]) -> Output {
    Command::new(BINARY)
        .arg("search")
        .args(archives)
        .args(args)
        .output()
        .expect("clp-s-rs can be spawned")
}

#[test]
fn every_fixture_query_matches_the_cpp_oracle_through_the_binary() -> anyhow::Result<()> {
    let root = prototype_root();
    let corpus = Corpus::load(&root)?;
    let registry = Registry::load(&root)?;
    let cpp = CppClpS::default();
    let reader = RustCliReader::new(PathBuf::from(BINARY));
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
    let failures: Vec<String> = harness
        .run_queries()
        .iter()
        .filter(|report| report.verdict.is_failure())
        .map(|report| report.case.clone())
        .collect();
    assert_eq!(failures, Vec::<String>::new());
    Ok(())
}

#[test]
fn options_mirror_the_cpp_search() {
    let archive = fixture("search.ts");
    let output = search(
        &[&archive],
        &[
            "--query",
            "level: INFO",
            "--tge",
            "1700000030000",
            "--projection",
            "rid",
            "level",
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    let expected: Vec<Vec<u8>> = query_oracle_lines("time-lower-and-filter")
        .iter()
        .map(|line| {
            let record: serde_json::Value =
                serde_json::from_slice(line).expect("the oracle holds JSON");
            serde_json::to_vec(&serde_json::json!({
                "rid": record["rid"],
                "level": record["level"],
            }))
            .expect("JSON serializes")
        })
        .collect();
    let mut expected = expected;
    expected.sort();
    assert_eq!(sorted_lines(&output.stdout), expected);

    let output = search(&[&archive], &["-q", "msg: \"*mixed case*\"", "-i"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        sorted_lines(&output.stdout),
        query_oracle_lines("ci-clp-substr")
    );

    let output = search(&[&archive], &["-q", "*", "--time-end-ms", "1700000001000"]);
    assert_eq!(
        sorted_lines(&output.stdout),
        query_oracle_lines("time-upper")
    );
}

#[test]
fn directories_and_several_inputs_form_one_session() -> anyhow::Result<()> {
    let work = tempfile::tempdir()?;
    let archives = work.path().join("archives");
    std::fs::create_dir(&archives)?;
    for index in 0..5 {
        let name = format!("logs.ts-split.{index}");
        std::os::unix::fs::symlink(fixture(&name), archives.join(format!("{name}.clps")))?;
    }
    let rest: Vec<PathBuf> = (5..9)
        .map(|index| fixture(&format!("logs.ts-split.{index}")))
        .collect();
    let mut inputs = vec![archives.as_path()];
    inputs.extend(rest.iter().map(PathBuf::as_path));
    let output = search(&inputs, &["-q", "level: ERROR AND service: auth"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        sorted_lines(&output.stdout),
        query_oracle_lines("logs-split-var")
    );
    Ok(())
}

#[test]
fn failures_map_to_the_documented_exit_codes() -> anyhow::Result<()> {
    let work = tempfile::tempdir()?;
    let archive = fixture("search.ts");
    let reject = prototype_root().join("tests/fixtures/archives/reject");
    for (archives, args, expected_exit) in [
        (vec![archive.clone()], vec!["-q", "msg: \"unterminated"], 1),
        (vec![archive.clone()], vec!["-q", "*", "--count"], 1),
        (
            vec![archive.clone()],
            vec!["-q", "*", "--count-by-time", "10"],
            1,
        ),
        (
            vec![archive.clone()],
            vec!["-q", "*", "--projection", "a.*"],
            1,
        ),
        (
            vec![archive.clone()],
            vec!["-q", "*", "--tge", "5", "--tle", "4"],
            1,
        ),
        (vec![archive.clone()], vec!["--query"], 1),
        (vec![work.path().join("missing.clps")], vec!["-q", "*"], 3),
        (vec![reject.join("scalars.mfa")], vec!["-q", "*"], 2),
        (
            vec![reject.join("arrays.structurize-arrays.clps")],
            vec!["-q", "*"],
            2,
        ),
        (vec![archive], vec!["-q", "level: NO_SUCH_LEVEL"], 0),
    ] {
        let paths: Vec<&Path> = archives.iter().map(PathBuf::as_path).collect();
        let output = search(&paths, &args);
        assert_eq!(
            output.status.code(),
            Some(expected_exit),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, Vec::<u8>::new(), "{args:?}");
        assert_eq!(0 == expected_exit, output.stderr.is_empty(), "{args:?}");
    }
    Ok(())
}

#[test]
fn archives_that_cannot_match_succeed_with_a_diagnostic() {
    for (archive, args, diagnostic) in [
        (
            fixture("search.default"),
            vec!["-q", "level: WARN", "--tge", "1"],
            "no authoritative timestamp column",
        ),
        (fixture("search.ts"), vec!["-q", "NOT *"], "logically false"),
    ] {
        let output = search(&[&archive], &args);
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        assert_eq!(output.stdout, Vec::<u8>::new(), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(diagnostic), "{args:?}: {stderr}");
        assert!(!stderr.contains('\u{1b}'), "{args:?}: {stderr:?}");
    }
}

#[test]
fn a_failing_archive_fails_the_session_after_the_earlier_results() -> anyhow::Result<()> {
    let work = tempfile::tempdir()?;
    let empty = work.path().join("empty.clps");
    std::fs::write(&empty, b"")?;
    let output = search(&[&fixture("search.ts"), &empty], &["-q", "level: INFO"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(sorted_lines(&output.stdout), query_oracle_lines("eq-var"));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("empty.clps"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
