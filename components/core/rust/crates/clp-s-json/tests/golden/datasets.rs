//! Format compatibility on the three dataset archives (large, under `PROTO_DATA_ROOT`): every
//! record renders to the bytes of a record of the C++ `clp-s x` oracle when emulating C++ plain
//! floats, compared as multisets of hashes. Ignored by default; run with
//! `cargo test --release -p clp-s-json -- --ignored datasets`.

use std::collections::HashMap;
use std::hash::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::io::BufRead;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use clp_s_table::DEFAULT_BATCH_NUM_ROWS;
use clp_s_table::ResolvedProjection;

use crate::support::EMULATED;
use crate::support::OpenedArchive;
use crate::support::fixtures_dir;

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
/// The hash of a record.
fn hash(record: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    record.hash(&mut hasher);
    hasher.finish()
}

#[test]
#[ignore = "reads the large dataset archives; run in release mode"]
fn datasets_render_the_cpp_bytes() -> anyhow::Result<()> {
    for dataset in datasets()? {
        let mut counts: HashMap<u64, i64> = HashMap::new();
        let archive = OpenedArchive::open(&dataset.archive)?;
        let mut num_rendered = 0_u64;
        archive.for_each_record(
            &ResolvedProjection::Full,
            EMULATED,
            DEFAULT_BATCH_NUM_ROWS,
            |record| {
                *counts.entry(hash(record)).or_default() += 1;
                num_rendered += 1;
            },
        )?;
        let oracle = std::io::BufReader::new(
            std::fs::File::open(&dataset.x_oracle)
                .with_context(|| format!("open {}", dataset.x_oracle.display()))?,
        );
        let mut num_expected = 0_u64;
        for line in oracle.split(b'\n') {
            let line = line?;
            if line.is_empty() {
                continue;
            }
            *counts.entry(hash(&line)).or_default() -= 1;
            num_expected += 1;
        }
        let num_unmatched: i64 = counts.values().map(|count| count.abs()).sum();
        assert_eq!(
            num_rendered, num_expected,
            "{}: record counts",
            dataset.name
        );
        assert_eq!(num_unmatched, 0, "{}: unmatched records", dataset.name);
        println!(
            "{}: {num_rendered} records match the C++ oracle byte for byte",
            dataset.name
        );
    }
    Ok(())
}
