//! `clp-s-rs extract` as a process: fixture equivalence through the differential harness, the
//! exit codes, append semantics, stdout output and directory inputs.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

use clp_s_difftest::corpus::Corpus;
use clp_s_difftest::corpus::prototype_root;
use clp_s_difftest::cpp::CppClpS;
use clp_s_difftest::harness::Harness;
use clp_s_difftest::harness::HarnessOptions;
use clp_s_difftest::harness::Verdict;
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
/// The sorted C++ `x` oracle lines of fixture `name`.
fn oracle_lines(name: &str) -> Vec<Vec<u8>> {
    sorted_lines(
        &std::fs::read(
            prototype_root()
                .join("tests/fixtures/oracles")
                .join(format!("{name}.jsonl")),
        )
        .expect("the oracle is readable"),
    )
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
/// The output of `clp-s-rs` run with `args`.
fn run(args: &[&Path]) -> Output {
    Command::new(BINARY)
        .args(args)
        .output()
        .expect("clp-s-rs can be spawned")
}

/// # Returns
///
/// The output of `clp-s-rs x <archives...> -o <output_dir>`.
fn extract(archives: &[&Path], output_dir: &Path) -> Output {
    let mut args = vec![Path::new("x")];
    args.extend_from_slice(archives);
    args.extend([Path::new("-o"), output_dir]);
    run(&args)
}

/// # Returns
///
/// The sorted lines of `output_dir/original`.
fn original_lines(output_dir: &Path) -> Vec<Vec<u8>> {
    sorted_lines(&std::fs::read(output_dir.join("original")).expect("the output exists"))
}

#[test]
fn every_fixture_matches_the_cpp_oracle_through_the_binary() -> anyhow::Result<()> {
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
    let num_passed = reports
        .iter()
        .filter(|report| Verdict::Pass == report.verdict)
        .count();
    assert_eq!(num_passed, 52);
    Ok(())
}

#[test]
fn failures_map_to_the_documented_exit_codes() -> anyhow::Result<()> {
    let work = tempfile::tempdir()?;
    let reject = prototype_root().join("tests/fixtures/archives/reject");
    let output_dir = work.path().join("out");
    for (archive, expected_exit) in [
        (work.path().join("missing.clps"), 3),
        (reject.join("scalars.mfa"), 2),
        (reject.join("arrays.structurize-arrays.clps"), 2),
    ] {
        let output = extract(&[&archive], &output_dir);
        assert_eq!(
            output.status.code(),
            Some(expected_exit),
            "{}: {}",
            archive.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.stderr.is_empty());
    }
    let output = extract(&[&reject.join("scalars.mfa")], &output_dir);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("multi-file archive"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output_dir.is_dir());
    assert!(
        !output_dir.join("original").try_exists()?,
        "a failing archive creates no output file"
    );

    let empty = work.path().join("empty.clps");
    std::fs::write(&empty, b"")?;
    let output = extract(&[&empty, &fixture("keys.default")], &output_dir);
    assert_eq!(output.status.code(), Some(2));
    assert!(!output_dir.join("original").try_exists()?);

    let archive = fixture("logs.ts");
    for (args, expected_exit) in [
        (vec![Path::new("x"), &archive, Path::new("-o")], 1),
        (vec![Path::new("x"), &archive], 1),
        (vec![Path::new("bogus")], 1),
        (
            vec![
                Path::new("x"),
                &archive,
                Path::new("-o"),
                &output_dir,
                Path::new("--ordered"),
            ],
            1,
        ),
        (vec![Path::new("--help")], 0),
        (vec![Path::new("x"), Path::new("--help")], 0),
        (
            vec![Path::new("s"), &archive, Path::new("-q"), Path::new("*")],
            0,
        ),
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(expected_exit), "{args:?}");
    }
    Ok(())
}

#[test]
fn extract_appends_to_the_original_file() -> anyhow::Result<()> {
    let work = tempfile::tempdir()?;
    let output_dir = work.path().join("nested/out");
    let archive = fixture("keys.default");
    for _ in 0..2 {
        let output = extract(&[&archive], &output_dir);
        assert_eq!(output.status.code(), Some(0));
    }
    let mut expected: Vec<Vec<u8>> = oracle_lines("keys.default")
        .into_iter()
        .flat_map(|line| [line.clone(), line])
        .collect();
    expected.sort();
    assert_eq!(original_lines(&output_dir), expected);
    Ok(())
}

#[test]
fn inputs_sharing_an_archive_id_are_each_extracted() -> anyhow::Result<()> {
    let work = tempfile::tempdir()?;
    let names = ["keys.default", "nested.default"];
    let mut archives = Vec::new();
    for name in names {
        let directory = work.path().join(name);
        std::fs::create_dir(&directory)?;
        let archive = directory.join("archive.clps");
        std::os::unix::fs::symlink(fixture(name), &archive)?;
        archives.push(archive);
    }
    let output_dir = work.path().join("out");
    let output = extract(
        &archives.iter().map(PathBuf::as_path).collect::<Vec<_>>(),
        &output_dir,
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut expected: Vec<Vec<u8>> = names.iter().flat_map(|name| oracle_lines(name)).collect();
    expected.sort();
    assert_eq!(original_lines(&output_dir), expected);
    Ok(())
}

#[test]
fn a_dash_output_directory_writes_to_stdout() {
    let output = extract(&[&fixture("strings.default")], Path::new("-"));
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        sorted_lines(&output.stdout),
        oracle_lines("strings.default")
    );
}

#[test]
fn directories_hold_one_archive_per_entry() -> anyhow::Result<()> {
    let work = tempfile::tempdir()?;
    let archives = work.path().join("archives");
    std::fs::create_dir(&archives)?;
    let names = ["logs.ts-split.4", "logs.ts-split.7"];
    for name in names {
        std::os::unix::fs::symlink(fixture(name), archives.join(format!("{name}.clps")))?;
    }

    let all = work.path().join("all");
    let output = extract(&[&archives, &fixture("nested.default")], &all);
    assert_eq!(output.status.code(), Some(0));
    let mut expected: Vec<Vec<u8>> = names
        .iter()
        .chain(&["nested.default"])
        .flat_map(|name| oracle_lines(name))
        .collect();
    expected.sort();
    assert_eq!(original_lines(&all), expected);

    let selected = work.path().join("selected");
    let output = run(&[
        Path::new("x"),
        &archives,
        Path::new("--archive-id"),
        Path::new("logs.ts-split.7.clps"),
        Path::new("-o"),
        &selected,
    ]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(original_lines(&selected), oracle_lines("logs.ts-split.7"));

    std::fs::create_dir(archives.join("subdirectory"))?;
    let output = extract(&[&archives], &work.path().join("nested"));
    assert_eq!(output.status.code(), Some(2));
    Ok(())
}
