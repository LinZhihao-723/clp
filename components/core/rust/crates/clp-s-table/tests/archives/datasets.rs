//! Format compatibility on the three dataset archives (large, under `PROTO_DATA_ROOT`): every
//! record decodes to a record of the C++ `clp-s x` oracle, compared as multisets of hashes.
//! Ignored by default; run with `cargo test --release -- --ignored datasets`.

use std::collections::HashMap;
use std::hash::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::io::BufRead;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;

use crate::json::Json;
use crate::support::OpenedArchive;
use crate::support::fixtures_dir;
use crate::support::render_archive;

/// The datasets of `tests/queries/datasets.toml` (its other tables and fields are ignored).
#[derive(Debug, serde::Deserialize)]
struct Manifest {
    dataset: Vec<Dataset>,
}

/// One dataset of `tests/queries/datasets.toml`.
#[derive(Debug, serde::Deserialize)]
struct Dataset {
    name: String,
    archive: PathBuf,
    x_oracle: PathBuf,
}

/// # Returns
///
/// The directory of large artifacts: `$PROTO_DATA_ROOT`, else the workspace's `target/proto-data`.
fn data_root() -> PathBuf {
    std::env::var_os("PROTO_DATA_ROOT").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/proto-data"),
        PathBuf::from,
    )
}

/// # Returns
///
/// Every dataset of `tests/queries/datasets.toml`, with paths under [`data_root`], on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if the manifest does not list three datasets.
/// * Forwards [`std::fs::read_to_string`]'s return values on failure.
/// * Forwards [`toml::from_str`]'s return values on failure.
fn datasets() -> anyhow::Result<Vec<Dataset>> {
    let manifest: Manifest = toml::from_str(&std::fs::read_to_string(
        fixtures_dir().join("../queries/datasets.toml"),
    )?)?;
    anyhow::ensure!(
        3 == manifest.dataset.len(),
        "three datasets: {:?}",
        manifest.dataset
    );
    let data_root = data_root();
    Ok(manifest
        .dataset
        .into_iter()
        .map(|dataset| Dataset {
            archive: data_root.join(dataset.archive),
            x_oracle: data_root.join(dataset.x_oracle),
            ..dataset
        })
        .collect())
}

/// # Returns
///
/// The hash of a serialized record.
fn hash(record: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    record.hash(&mut hasher);
    hasher.finish()
}

#[test]
#[ignore = "reads the large dataset archives; run in release mode"]
fn datasets_decode_to_the_cpp_records() -> anyhow::Result<()> {
    for Dataset {
        name,
        archive: archive_path,
        x_oracle: oracle_path,
    } in datasets()?
    {
        let mut counts: HashMap<u64, i64> = HashMap::new();
        let archive = OpenedArchive::open(&archive_path)?;
        let rendered = render_archive(&archive, 4096)?;
        let num_rendered = rendered.len();
        for record in rendered {
            *counts.entry(hash(&record.serialize())).or_default() += 1;
        }
        let oracle = std::io::BufReader::new(
            std::fs::File::open(&oracle_path)
                .with_context(|| format!("open {}", oracle_path.display()))?,
        );
        let mut num_expected = 0;
        for line in oracle.split(b'\n') {
            let line = line?;
            if line.is_empty() {
                continue;
            }
            *counts
                .entry(hash(&Json::parse(&line)?.serialize()))
                .or_default() -= 1;
            num_expected += 1;
        }
        let num_unmatched: i64 = counts.values().map(|count| count.abs()).sum();
        assert_eq!(num_rendered, num_expected, "{name}: record counts");
        assert_eq!(num_unmatched, 0, "{name}: unmatched records");
        println!("{name}: {num_rendered} records match the C++ oracle");
    }
    Ok(())
}
