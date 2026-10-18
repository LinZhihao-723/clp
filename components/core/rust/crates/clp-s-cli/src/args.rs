//! The command-line surface.

use std::path::PathBuf;

/// The Rust clp-s archive reader.
#[derive(Debug, clap::Parser)]
#[command(name = "clp-s-rs", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// A subcommand.
#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Decompresses archives into `OUTPUT_DIR/original` as JSON lines (unordered, appended like
    /// C++).
    #[command(alias = "x")]
    Extract(ExtractArgs),

    /// Searches archives with a KQL query.
    #[command(alias = "s")]
    Search(Box<SearchArgs>),
}

/// How archive inputs are read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Auth {
    /// Local paths.
    #[default]
    None,

    /// S3 object URLs (`s3://bucket/key`, or virtual-hosted or path-style `http(s)://` URLs like
    /// C++), with credentials from the default AWS chain; inputs that exist locally stay local.
    S3,
}

/// The archive inputs shared by every subcommand.
#[derive(Debug, clap::Args)]
pub struct ArchiveArgs {
    /// Single-file archives, directories of single-file archives, or (with `--auth s3`) S3 object
    /// URLs.
    #[arg(required = true)]
    pub archive_paths: Vec<String>,

    /// Limit the input to the archive with this id inside each directory.
    #[arg(long)]
    pub archive_id: Option<String>,

    #[arg(long, value_enum, default_value_t = Auth::None)]
    pub auth: Auth,

    /// The S3 region (with `--auth s3`).
    #[arg(long)]
    pub s3_region: Option<String>,

    /// A custom S3 endpoint, addressed path-style (with `--auth s3`).
    #[arg(long)]
    pub s3_endpoint_url: Option<String>,
}

/// Arguments of `extract`.
#[derive(Debug, clap::Args)]
pub struct ExtractArgs {
    #[command(flatten)]
    pub archives: ArchiveArgs,

    /// The output directory.
    #[arg(long, short = 'o')]
    pub output_dir: PathBuf,

    /// Rejected: ordered decompression is not supported by this reader.
    #[arg(long, hide = true)]
    pub ordered: bool,
}

/// Where search results go.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputKind {
    /// JSON lines on stdout.
    #[default]
    Stdout,

    /// msgpack result tuples into `--path`, created (or truncated) once per session.
    File,

    /// msgpack result tuples to `--host`:`--port`, over one connection per session.
    Network,

    /// `MongoDB` documents into `--uri`/`--collection`.
    ResultsCache,
}

/// Arguments of `search`.
#[derive(Debug, clap::Args)]
pub struct SearchArgs {
    #[command(flatten)]
    pub archives: ArchiveArgs,

    /// The KQL query.
    #[arg(long, short = 'q')]
    pub query: String,

    /// Match records whose timestamp is at least this many epoch milliseconds.
    #[arg(long, visible_alias = "tge", allow_negative_numbers = true)]
    pub time_begin_ms: Option<i64>,

    /// Match records whose timestamp is at most this many epoch milliseconds.
    #[arg(long, visible_alias = "tle", allow_negative_numbers = true)]
    pub time_end_ms: Option<i64>,

    /// Ignore ASCII case distinctions between query values and the data.
    #[arg(long, short = 'i')]
    pub ignore_case: bool,

    /// Render only these columns of each matched record.
    #[arg(long, num_args = 1..)]
    pub projection: Vec<String>,

    #[arg(long, value_enum, default_value_t = OutputKind::Stdout)]
    pub output: OutputKind,

    /// The output file (`--output file`).
    #[arg(long)]
    pub path: Option<PathBuf>,

    /// The network peer host (`--output network`).
    #[arg(long)]
    pub host: Option<String>,

    /// The network peer port (`--output network`).
    #[arg(long)]
    pub port: Option<u16>,

    /// The `MongoDB` URI, including the database (`--output results-cache`).
    #[arg(long)]
    pub uri: Option<String>,

    /// The collection (`--output results-cache`).
    #[arg(long)]
    pub collection: Option<String>,

    /// The dataset stored with each result (`--output results-cache`; empty by default, like C++).
    #[arg(long, default_value = "")]
    pub dataset: String,

    /// Documents per insert (`--output results-cache`).
    #[arg(long, default_value_t = 1000)]
    pub batch_size: usize,

    /// Keep only the latest N results across all archives.
    #[arg(long)]
    pub max_num_results: Option<std::num::NonZeroU32>,

    /// Rejected: aggregations are not supported by this reader.
    #[arg(long, hide = true)]
    pub count: bool,

    /// Rejected: aggregations are not supported by this reader.
    #[arg(long, hide = true)]
    pub count_by_time: Option<i64>,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use clap::Parser;

    use super::Auth;
    use super::Cli;
    use super::Command;
    use super::OutputKind;

    #[test]
    fn command_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn search_accepts_cpp_style_aliases() -> anyhow::Result<()> {
        let cli = Cli::try_parse_from([
            "clp-s-rs",
            "s",
            "archives/a.clps",
            "--query",
            "level: ERROR",
            "--tge",
            "-5",
            "--tle",
            "1735694700000",
            "-i",
            "--projection",
            "a.b",
            "c",
        ])?;
        let Command::Search(search) = cli.command else {
            anyhow::bail!("expected the search subcommand, got {:?}", cli.command);
        };
        assert_eq!(search.archives.archive_paths, ["archives/a.clps"]);
        assert_eq!(search.query, "level: ERROR");
        assert_eq!(search.time_begin_ms, Some(-5));
        assert_eq!(search.time_end_ms, Some(1_735_694_700_000));
        assert!(search.ignore_case);
        assert_eq!(search.projection, ["a.b", "c"]);
        assert_eq!(search.output, OutputKind::Stdout);
        assert_eq!(search.archives.auth, Auth::None);
        Ok(())
    }

    #[test]
    fn extract_requires_an_output_directory() -> anyhow::Result<()> {
        assert!(Cli::try_parse_from(["clp-s-rs", "x", "a.clps"]).is_err());
        let cli = Cli::try_parse_from(["clp-s-rs", "x", "a.clps", "b.clps", "-o", "out"])?;
        let Command::Extract(extract) = cli.command else {
            anyhow::bail!("expected the extract subcommand, got {:?}", cli.command);
        };
        assert_eq!(extract.archives.archive_paths, ["a.clps", "b.clps"]);
        assert_eq!(extract.output_dir, std::path::PathBuf::from("out"));
        assert!(!extract.ordered);
        Ok(())
    }
}
