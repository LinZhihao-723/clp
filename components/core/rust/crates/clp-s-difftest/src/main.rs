//! `clp-s-difftest`: runs the fixture, query-suite and dataset comparisons, and the query
//! performance report.

use std::io::IsTerminal;
use std::path::PathBuf;

use clap::Parser;
use clp_s_difftest::corpus::Corpus;
use clp_s_difftest::corpus::data_root;
use clp_s_difftest::corpus::prototype_root;
use clp_s_difftest::cpp::CppClpS;
use clp_s_difftest::cpp::CppReader;
use clp_s_difftest::cpp::default_cpp_clp_s;
use clp_s_difftest::harness::CaseReport;
use clp_s_difftest::harness::DEFAULT_MAX_DIFF_RECORDS;
use clp_s_difftest::harness::DEFAULT_MAX_PROBED_RECORDS;
use clp_s_difftest::harness::Harness;
use clp_s_difftest::harness::HarnessOptions;
use clp_s_difftest::harness::Summary;
use clp_s_difftest::perf::PerfOptions;
use clp_s_difftest::perf::run_perf;
use clp_s_difftest::perf::utc_date_today;
use clp_s_difftest::reader::ReaderUnderTest;
use clp_s_difftest::registry::Registry;
use clp_s_difftest::rust::RUST_CLI_BINARY_NAME;
use clp_s_difftest::rust::RustCliReader;
use clp_s_difftest::rust::RustLibraryReader;
use clp_s_difftest::self_test::is_consistent_cpp_verdict;
use clp_s_reader::JsonOptions;

/// The differential harness of the Rust clp-s reader.
#[derive(Debug, clap::Parser)]
#[command(name = "clp-s-difftest")]
struct Cli {
    /// The C++ `clp-s` binary (default: `$CLP_S`, else `build/core/clp-s` of this repository).
    #[arg(long)]
    cpp_clp_s: Option<PathBuf>,

    /// The directory of large artifacts (default: `$PROTO_DATA_ROOT`, else the workspace's
    /// `target/proto-data`).
    #[arg(long)]
    data_root: Option<PathBuf>,

    /// The scratch directory (defaults to a fresh directory under `<data-root>/work`).
    #[arg(long)]
    work_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

/// A subcommand.
#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Decompresses every fixture archive and compares it with the C++ `x` oracle.
    Fixtures(RunArgs),

    /// Runs the fixture queries of the suite.
    Queries(RunArgs),

    /// Decompresses the datasets and runs the dataset queries.
    Datasets(RunArgs),

    /// Measures wall time and peak RSS of C++ and Rust on the dataset queries.
    Perf(PerfArgs),
}

/// The implementation under test.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ReaderChoice {
    /// The C++ `clp-s` itself: the harness self-test against its own oracles.
    Cpp,

    /// The Rust reader library (extract through the pull cursor, search through a session).
    RustLibrary,

    /// The `clp-s-rs` binary.
    RustCli,
}

/// Arguments of the comparison subcommands.
#[derive(Debug, clap::Args)]
struct RunArgs {
    #[arg(long, value_enum, default_value_t = ReaderChoice::Cpp)]
    reader: ReaderChoice,

    /// The `clp-s-rs` binary of `--reader rust-cli` (default: next to this binary).
    #[arg(long)]
    rust_cli: Option<PathBuf>,

    /// Run only these cases (query ids, fixture names, archive names or dataset names).
    #[arg(long = "case")]
    cases: Vec<String>,

    /// The number of records of each side a failure shows.
    #[arg(long, default_value_t = DEFAULT_MAX_DIFF_RECORDS)]
    max_diff_records: usize,

    /// Re-run C++ on single-record archives for undeclared mismatches (design report §8.3).
    #[arg(long)]
    probe: bool,

    /// Skip checking stdout results against `clp_s_query`'s reference evaluator.
    #[arg(long)]
    no_reference: bool,

    /// Print only failures and the summary.
    #[arg(long)]
    quiet: bool,
}

/// Arguments of `perf`.
#[derive(Debug, clap::Args)]
struct PerfArgs {
    /// The `clp-s-rs` binary; without it only C++ is measured.
    #[arg(long)]
    rust_cli: Option<PathBuf>,

    /// Measured runs per case, after one warm-up run.
    #[arg(long, default_value_t = 5)]
    runs: u32,

    /// Pin runs with `taskset -c` (for example `8-15`).
    #[arg(long)]
    cpus: Option<String>,

    /// Run only these cases (query ids or dataset names).
    #[arg(long = "case")]
    cases: Vec<String>,

    /// Skip measuring `x`.
    #[arg(long)]
    skip_extract: bool,
}

/// A comparison subcommand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Fixtures,
    Queries,
    Datasets,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    let data_root = cli.data_root.clone().unwrap_or_else(data_root);
    let corpus = Corpus::load_with_data_root(&prototype_root(), &data_root)?;
    let work_root = cli
        .work_dir
        .clone()
        .unwrap_or_else(|| data_root.join("work"));
    std::fs::create_dir_all(&work_root)?;
    let work = tempfile::Builder::new()
        .prefix("difftest-")
        .tempdir_in(&work_root)?;
    let cpp = CppClpS {
        binary: cli.cpp_clp_s.unwrap_or_else(default_cpp_clp_s),
    };
    match cli.command {
        Command::Fixtures(args) => run(&corpus, &cpp, work.path(), &args, Stage::Fixtures),
        Command::Queries(args) => run(&corpus, &cpp, work.path(), &args, Stage::Queries),
        Command::Datasets(args) => run(&corpus, &cpp, work.path(), &args, Stage::Datasets),
        Command::Perf(args) => {
            let options = PerfOptions {
                cpp_binary: cpp.binary,
                rust_binary: args.rust_cli,
                num_runs: args.runs,
                cpu_list: args.cpus,
                case_filter: args.cases,
                is_extract_measured: !args.skip_extract,
            };
            let report = run_perf(&corpus, &options, work.path())?;
            let perf_dir = data_root.join("perf");
            std::fs::create_dir_all(&perf_dir)?;
            let path = perf_dir.join(format!("{}.json", utc_date_today()));
            std::fs::write(&path, serde_json::to_vec_pretty(&report)?)?;
            println!("{}", report.render_markdown());
            println!("raw results: {}", path.display());
            Ok(())
        }
    }
}

/// Runs one comparison stage with the chosen reader and prints its reports.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if the chosen reader is not wired yet, or a case fails.
/// * Forwards [`Registry::load`]'s return values on failure.
fn run(
    corpus: &Corpus,
    cpp: &CppClpS,
    work_dir: &std::path::Path,
    args: &RunArgs,
    stage: Stage,
) -> anyhow::Result<()> {
    let registry = Registry::load(&corpus.root)?;
    let options = HarnessOptions {
        max_diff_records: args.max_diff_records,
        is_archive_dependence_probe_enabled: args.probe,
        max_probed_records: DEFAULT_MAX_PROBED_RECORDS,
        is_reference_evaluator_enabled: !args.no_reference,
        case_filter: args.cases.clone(),
    };
    match args.reader {
        ReaderChoice::Cpp => {
            let reader = CppReader::new(cpp.clone());
            let harness = Harness::new(corpus, &registry, cpp, &reader, work_dir, options);
            let reports = run_stage(&harness, stage);
            print_reports(&reports, args.quiet);
            let num_inconsistent = reports
                .iter()
                .filter(|report| !is_consistent_cpp_verdict(corpus, report))
                .inspect(|report| println!("SELF-TEST INCONSISTENT: {report}"))
                .count();
            println!(
                "self-test: {} of {} verdicts consistent with C++ reproducing its own oracles",
                reports.len() - num_inconsistent,
                reports.len()
            );
            if num_inconsistent > 0 {
                anyhow::bail!("{num_inconsistent} self-test verdicts are inconsistent");
            }
            Ok(())
        }
        ReaderChoice::RustLibrary => {
            let reader = RustLibraryReader::new(JsonOptions::default())?;
            let harness = Harness::new(corpus, &registry, cpp, &reader, work_dir, options);
            run_rust_stage(&harness, stage, args.quiet)
        }
        ReaderChoice::RustCli => {
            let binary = match &args.rust_cli {
                Some(binary) => binary.clone(),
                None => std::env::current_exe()?.with_file_name(RUST_CLI_BINARY_NAME),
            };
            let reader = RustCliReader::new(binary);
            let harness = Harness::new(corpus, &registry, cpp, &reader, work_dir, options);
            run_rust_stage(&harness, stage, args.quiet)
        }
    }
}

/// Runs one comparison stage with a Rust reader and prints its reports.
///
/// # Type Parameters
///
/// * `ReaderType` - The Rust reader under test.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if a case fails.
fn run_rust_stage<ReaderType: ReaderUnderTest>(
    harness: &Harness<'_, ReaderType>,
    stage: Stage,
    is_quiet: bool,
) -> anyhow::Result<()> {
    let reports = run_stage(harness, stage);
    print_reports(&reports, is_quiet);
    let num_failed = Summary::of(&reports).num_failed;
    if num_failed > 0 {
        anyhow::bail!("{num_failed} cases failed");
    }
    Ok(())
}

/// # Type Parameters
///
/// * `ReaderType` - The reader under test.
///
/// # Returns
///
/// The reports of `stage`.
fn run_stage<ReaderType: ReaderUnderTest>(
    harness: &Harness<'_, ReaderType>,
    stage: Stage,
) -> Vec<CaseReport> {
    match stage {
        Stage::Fixtures => harness.run_fixtures(),
        Stage::Queries => harness.run_queries(),
        Stage::Datasets => harness.run_datasets(),
    }
}

/// Prints every report (only failures when `is_quiet`), then the summary.
fn print_reports(reports: &[CaseReport], is_quiet: bool) {
    for report in reports {
        if !is_quiet || report.verdict.is_failure() {
            println!("{report}");
        }
    }
    println!("{}", Summary::of(reports));
}
