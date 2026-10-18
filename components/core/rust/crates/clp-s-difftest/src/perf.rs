//! The query-performance report (`docs/DESIGN.md` §11.5): wall time and peak RSS of C++ `clp-s` and
//! `clp-s-rs` on every dataset query and on `x`, measured with `/usr/bin/time`.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Instant;

use crate::corpus::Corpus;
use crate::corpus::QueryCase;
use crate::corpus::QueryTarget;
use crate::cpp::exit_code;

/// The GNU time binary used for peak RSS.
pub const TIME_BINARY: &str = "/usr/bin/time";

/// Perf options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PerfOptions {
    /// The C++ `clp-s` binary.
    pub cpp_binary: PathBuf,

    /// The `clp-s-rs` binary, if the Rust side is measured.
    pub rust_binary: Option<PathBuf>,

    /// Measured runs per (tool, case, sink), after one warm-up run.
    pub num_runs: u32,

    /// The `taskset -c` CPU list, if pinned.
    pub cpu_list: Option<String>,

    /// The query ids or dataset names to run; empty runs every dataset case.
    pub case_filter: Vec<String>,

    /// Also measure `x` of every dataset.
    pub is_extract_measured: bool,
}

/// Where a measured run writes its results.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PerfSink {
    /// Stdout to `/dev/null`.
    DevNull,

    /// Stdout to a file.
    File,

    /// `x` into an output directory.
    Directory,
}

/// The measurements of one (tool, case, sink).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PerfMeasurement {
    /// `cpp` or `rust`.
    pub tool: String,

    /// The query id, or `x:<dataset>`.
    pub case: String,

    pub sink: PerfSink,
    pub wall_seconds: Vec<f64>,
    pub peak_rss_kib: Vec<u64>,

    /// The exit code of the last run.
    pub exit_code: i32,
}

impl PerfMeasurement {
    /// # Returns
    ///
    /// The median wall time in seconds (the mean of the two middle runs for an even count; 0
    /// without runs).
    #[must_use]
    pub fn median_wall_seconds(&self) -> f64 {
        let mut sorted = self.wall_seconds.clone();
        sorted.sort_by(f64::total_cmp);
        let middle = sorted.len() / 2;
        match sorted.len() {
            0 => 0.0,
            length if length % 2 == 0 => f64::midpoint(sorted[middle - 1], sorted[middle]),
            _ => sorted[middle],
        }
    }

    /// # Returns
    ///
    /// The (min, max) wall time in seconds (0 without runs).
    #[must_use]
    pub fn wall_seconds_range(&self) -> (f64, f64) {
        let min = self.wall_seconds.iter().copied().reduce(f64::min);
        let max = self.wall_seconds.iter().copied().reduce(f64::max);
        (min.unwrap_or_default(), max.unwrap_or_default())
    }

    /// # Returns
    ///
    /// The median peak RSS in KiB (0 without runs).
    #[must_use]
    pub fn median_peak_rss_kib(&self) -> u64 {
        let mut sorted = self.peak_rss_kib.clone();
        sorted.sort_unstable();
        sorted.get(sorted.len() / 2).copied().unwrap_or_default()
    }
}

/// The whole report.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct PerfReport {
    pub measurements: Vec<PerfMeasurement>,
}

impl PerfReport {
    /// Renders the report as a Markdown table: one row per (case, sink), C++ and Rust side by
    /// side.
    ///
    /// # Returns
    ///
    /// The table.
    #[must_use]
    pub fn render_markdown(&self) -> String {
        let mut lines = vec![
            "| case | sink | C++ wall s (median [min-max]) | C++ RSS MiB | Rust wall s (median \
             [min-max]) | Rust RSS MiB | Rust/C++ |"
                .to_owned(),
            "|---|---|---|---|---|---|---|".to_owned(),
        ];
        for cpp in self
            .measurements
            .iter()
            .filter(|measurement| measurement.tool == "cpp")
        {
            let rust = self.measurements.iter().find(|measurement| {
                measurement.tool == "rust"
                    && measurement.case == cpp.case
                    && measurement.sink == cpp.sink
            });
            let (rust_wall, rust_rss, ratio) = rust.map_or_else(
                || ("-".to_owned(), "-".to_owned(), "-".to_owned()),
                |rust| {
                    (
                        render_wall(rust),
                        render_rss(rust),
                        format!(
                            "{:.2}",
                            rust.median_wall_seconds() / cpp.median_wall_seconds().max(1e-9)
                        ),
                    )
                },
            );
            lines.push(format!(
                "| {} | {:?} | {} | {} | {rust_wall} | {rust_rss} | {ratio} |",
                cpp.case,
                cpp.sink,
                render_wall(cpp),
                render_rss(cpp)
            ));
        }
        lines.join("\n")
    }
}

/// Measures every selected dataset case.
///
/// # Returns
///
/// The report on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if a dataset of a query is unknown.
/// * Forwards [`measure`]'s return values on failure.
pub fn run_perf(
    corpus: &Corpus,
    options: &PerfOptions,
    work_dir: &Path,
) -> anyhow::Result<PerfReport> {
    let is_selected = |name: &str| {
        options.case_filter.is_empty()
            || options.case_filter.iter().any(|selected| selected == name)
    };
    let mut report = PerfReport::default();
    if options.is_extract_measured {
        for dataset in corpus
            .datasets
            .iter()
            .filter(|dataset| is_selected(&dataset.name))
        {
            let case = format!("x:{}", dataset.name);
            let output_dir = work_dir.join("perf-x");
            let mut tools = vec![(
                "cpp",
                vec![
                    options.cpp_binary.clone().into_os_string(),
                    "x".into(),
                    dataset.archive.clone().into_os_string(),
                    output_dir.clone().into_os_string(),
                ],
            )];
            if let Some(rust_binary) = &options.rust_binary {
                tools.push((
                    "rust",
                    vec![
                        rust_binary.clone().into_os_string(),
                        "extract".into(),
                        dataset.archive.clone().into_os_string(),
                        "-o".into(),
                        output_dir.clone().into_os_string(),
                    ],
                ));
            }
            for (tool, arguments) in tools {
                report.measurements.push(measure(
                    tool,
                    &case,
                    PerfSink::Directory,
                    &arguments,
                    Some(&output_dir),
                    options,
                    work_dir,
                )?);
            }
        }
    }
    for case in &corpus.queries {
        let QueryTarget::Dataset(name) = &case.target else {
            continue;
        };
        if !is_selected(&case.id) && !is_selected(name) {
            continue;
        }
        let dataset = corpus
            .dataset(name)
            .ok_or_else(|| anyhow::anyhow!("query {} targets unknown dataset {name}", case.id))?;
        let mut tools = vec![(
            "cpp",
            cpp_search_arguments(&options.cpp_binary, &dataset.archive, case),
        )];
        if let Some(rust_binary) = &options.rust_binary {
            tools.push((
                "rust",
                rust_search_arguments(rust_binary, &dataset.archive, case),
            ));
        }
        for (tool, arguments) in tools {
            for sink in [PerfSink::DevNull, PerfSink::File] {
                report.measurements.push(measure(
                    tool, &case.id, sink, &arguments, None, options, work_dir,
                )?);
            }
        }
    }
    Ok(report)
}

/// # Returns
///
/// Today's UTC date as `YYYY-MM-DD`.
#[must_use]
pub fn utc_date_today() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    civil_date(i64::try_from(seconds / 86_400).unwrap_or_default())
}

/// # Returns
///
/// The proleptic Gregorian `YYYY-MM-DD` of a day count since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
#[must_use]
pub fn civil_date(days_since_epoch: i64) -> String {
    let shifted = days_since_epoch + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// # Returns
///
/// The C++ `clp-s s` command line of a case (as `scripts/corpus.py` builds it).
fn cpp_search_arguments(
    binary: &Path,
    archive: &Path,
    case: &QueryCase,
) -> Vec<std::ffi::OsString> {
    let mut arguments = vec![
        binary.as_os_str().to_owned(),
        "s".into(),
        archive.as_os_str().to_owned(),
        case.kql.clone().into(),
    ];
    arguments.extend(common_search_arguments(case));
    if !case.options.projection.is_empty() {
        arguments.push("--projection".into());
        arguments.extend(case.options.projection.iter().map(Into::into));
    }
    arguments
}

/// # Returns
///
/// The `clp-s-rs search` command line of a case (`docs/DESIGN.md` §9).
fn rust_search_arguments(
    binary: &Path,
    archive: &Path,
    case: &QueryCase,
) -> Vec<std::ffi::OsString> {
    let mut arguments = vec![
        binary.as_os_str().to_owned(),
        "search".into(),
        archive.as_os_str().to_owned(),
        "-q".into(),
        case.kql.clone().into(),
    ];
    arguments.extend(common_search_arguments(case));
    if !case.options.projection.is_empty() {
        arguments.push("--projection".into());
        arguments.extend(case.options.projection.iter().map(Into::into));
    }
    arguments
}

/// # Returns
///
/// The `-i`, `--tge` and `--tle` arguments of a case.
fn common_search_arguments(case: &QueryCase) -> Vec<std::ffi::OsString> {
    let mut arguments = Vec::new();
    if case.options.is_case_insensitive {
        arguments.push("-i".into());
    }
    if let Some(begin) = case.options.time_begin_ms {
        arguments.push("--tge".into());
        arguments.push(begin.to_string().into());
    }
    if let Some(end) = case.options.time_end_ms {
        arguments.push("--tle".into());
        arguments.push(end.to_string().into());
    }
    arguments
}

/// Runs a command once to warm the cache, then `options.num_runs` times under
/// `/usr/bin/time` (and `taskset`, if pinned).
///
/// # Returns
///
/// The measurements on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if a run cannot be spawned, its output file cannot be created, or the time
///   report cannot be read.
fn measure(
    tool: &str,
    case: &str,
    sink: PerfSink,
    arguments: &[std::ffi::OsString],
    output_dir: Option<&Path>,
    options: &PerfOptions,
    work_dir: &Path,
) -> anyhow::Result<PerfMeasurement> {
    tracing::info!(tool, case, sink = ? sink, "Measuring.");
    let time_report = work_dir.join("perf-time.txt");
    let stdout_file = work_dir.join("perf-stdout.jsonl");
    let mut measurement = PerfMeasurement {
        tool: tool.to_owned(),
        case: case.to_owned(),
        sink,
        wall_seconds: Vec::new(),
        peak_rss_kib: Vec::new(),
        exit_code: 0,
    };
    for run_index in 0..=options.num_runs {
        if let Some(output_dir) = output_dir.filter(|directory| directory.exists()) {
            std::fs::remove_dir_all(output_dir)?;
        }
        let mut command = Command::new(TIME_BINARY);
        command.arg("-f").arg("%M").arg("-o").arg(&time_report);
        if let Some(cpu_list) = &options.cpu_list {
            command.arg("taskset").arg("-c").arg(cpu_list);
        }
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        command.stdout(match sink {
            PerfSink::File => Stdio::from(std::fs::File::create(&stdout_file)?),
            PerfSink::DevNull | PerfSink::Directory => Stdio::null(),
        });
        let start = Instant::now();
        let status = command
            .status()
            .map_err(|error| anyhow::anyhow!("cannot run {TIME_BINARY}: {error}"))?;
        let wall_seconds = start.elapsed().as_secs_f64();
        measurement.exit_code = exit_code(status);
        let peak_rss_kib = std::fs::read_to_string(&time_report)?
            .lines()
            .filter_map(|line| line.trim().parse::<u64>().ok())
            .next_back()
            .ok_or_else(|| anyhow::anyhow!("no peak RSS in {}", time_report.display()))?;
        if run_index > 0 {
            measurement.wall_seconds.push(wall_seconds);
            measurement.peak_rss_kib.push(peak_rss_kib);
        }
    }
    if stdout_file.exists() {
        std::fs::remove_file(&stdout_file)?;
    }
    if let Some(output_dir) = output_dir.filter(|directory| directory.exists()) {
        std::fs::remove_dir_all(output_dir)?;
    }
    Ok(measurement)
}

/// # Returns
///
/// `median [min-max]` of the wall times.
fn render_wall(measurement: &PerfMeasurement) -> String {
    let (min, max) = measurement.wall_seconds_range();
    format!(
        "{:.3} [{min:.3}-{max:.3}]",
        measurement.median_wall_seconds()
    )
}

/// # Returns
///
/// The median peak RSS in MiB.
fn render_rss(measurement: &PerfMeasurement) -> String {
    let kib = u32::try_from(measurement.median_peak_rss_kib()).map_or(f64::MAX, f64::from);
    format!("{:.1}", kib / 1024.0)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::PerfMeasurement;
    use super::PerfOptions;
    use super::PerfReport;
    use super::PerfSink;
    use super::civil_date;
    use super::measure;

    const BASE_OPTIONS: PerfOptions = PerfOptions {
        cpp_binary: PathBuf::new(),
        rust_binary: None,
        num_runs: 3,
        cpu_list: None,
        case_filter: Vec::new(),
        is_extract_measured: false,
    };

    #[test]
    fn civil_dates_match_python() {
        for (days, date) in [
            (0, "1970-01-01"),
            (19_000, "2022-01-08"),
            (-1, "1969-12-31"),
            (20_743, "2026-10-17"),
            (11_016, "2000-02-29"),
        ] {
            assert_eq!(civil_date(days), date, "{days}");
        }
    }

    #[test]
    fn statistics_use_the_median_and_range() {
        let measurement = PerfMeasurement {
            tool: "cpp".to_owned(),
            case: "q".to_owned(),
            sink: PerfSink::DevNull,
            wall_seconds: vec![3.0, 1.0, 2.0],
            peak_rss_kib: vec![300, 100, 200],
            exit_code: 0,
        };
        assert!((measurement.median_wall_seconds() - 2.0).abs() < f64::EPSILON);
        let even = PerfMeasurement {
            wall_seconds: vec![4.0, 1.0, 2.0, 3.0],
            ..measurement.clone()
        };
        assert!((even.median_wall_seconds() - 2.5).abs() < f64::EPSILON);
        assert_eq!(measurement.wall_seconds_range(), (1.0, 3.0));
        assert_eq!(measurement.median_peak_rss_kib(), 200);
        let mut rust = measurement.clone();
        rust.tool = "rust".to_owned();
        rust.wall_seconds = vec![1.0];
        let report = PerfReport {
            measurements: vec![measurement, rust],
        };
        let table = report.render_markdown();
        assert!(
            table.contains(
                "| q | DevNull | 2.000 [1.000-3.000] | 0.2 | 1.000 [1.000-1.000] | 0.2 | 0.50 |"
            ),
            "{table}"
        );
    }

    #[test]
    fn measure_runs_the_command_after_a_warm_up() -> anyhow::Result<()> {
        let work = tempfile::tempdir()?;
        let options = PerfOptions {
            cpu_list: Some("0".to_owned()),
            ..BASE_OPTIONS
        };
        let measurement = measure(
            "cpp",
            "echo",
            PerfSink::File,
            &["echo".into(), "hello".into()],
            None,
            &options,
            work.path(),
        )?;
        assert_eq!(measurement.wall_seconds.len(), 3);
        assert_eq!(measurement.peak_rss_kib.len(), 3);
        assert!(measurement.peak_rss_kib.iter().all(|rss| *rss > 0));
        assert_eq!(measurement.exit_code, 0);
        Ok(())
    }
}
