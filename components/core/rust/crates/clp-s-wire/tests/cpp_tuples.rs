//! Byte compatibility with the C++ msgpack outputs: every tuple of every msgpack oracle of the
//! query suite, decoded and re-encoded, reproduces the bytes C++ wrote.

use std::path::Path;
use std::path::PathBuf;

use clp_s_wire::SearchResultTuple;

/// The C++ `file --path` oracles of the query suite.
const ORACLES: [&str; 4] = [
    "wire-msgpack-kvir",
    "wire-msgpack-nolog",
    "wire-msgpack-search",
    "wire-msgpack-split",
];

/// # Returns
///
/// The raw C++ msgpack oracle `id`.
fn oracle_path(id: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/queries/oracles/msgpack")
        .join(format!("{id}.msgpack"))
}

/// One decoded C++ tuple: `(timestamp_ms, message, orig_file_path, archive_id, log_event_idx)`.
type CppTuple = (i64, String, String, String, i64);

#[test]
fn every_cpp_tuple_is_reproduced_byte_for_byte() -> anyhow::Result<()> {
    let mut num_tuples = 0;
    for id in ORACLES {
        let data = std::fs::read(oracle_path(id))?;
        let mut remaining = data.as_slice();
        while !remaining.is_empty() {
            let size_before = remaining.len();
            let mut deserializer = rmp_serde::Deserializer::new(&mut remaining);
            let (timestamp_ms, message, orig_file_path, archive_id, log_event_idx): CppTuple =
                serde::Deserialize::deserialize(&mut deserializer)?;
            let cpp_bytes = &data[data.len() - size_before..data.len() - remaining.len()];
            let text = message
                .strip_suffix('\n')
                .expect("every C++ message ends with a newline");
            let mut encoded = Vec::new();
            SearchResultTuple {
                timestamp_ms,
                message: text.as_bytes(),
                orig_file_path: &orig_file_path,
                archive_id: &archive_id,
                log_event_idx,
            }
            .encode(&mut encoded)?;
            assert_eq!(encoded, cpp_bytes, "{id}: tuple {num_tuples}");
            num_tuples += 1;
        }
    }
    assert!(num_tuples > 20, "only {num_tuples} tuples were compared");
    Ok(())
}
