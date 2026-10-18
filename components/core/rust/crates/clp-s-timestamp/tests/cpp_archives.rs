//! Compiles every timestamp pattern stored in the C++ fixture archives.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Context;
use clp_s_timestamp::TimestampPattern;

const HEADER_SIZE: usize = 64;
const METADATA_SECTION_SIZE_OFFSET: usize = 56;
const TIMESTAMP_DICTIONARY_PACKET_TYPE: u8 = 2;
const EPOCH_ENCODING: u64 = 1;
const DOUBLE_EPOCH_ENCODING: u64 = 2;

/// A little-endian reader over a byte slice.
struct Reader<'bytes> {
    bytes: &'bytes [u8],
    position: usize,
}

impl<'bytes> Reader<'bytes> {
    /// # Returns
    ///
    /// The next `length` bytes on success.
    ///
    /// # Errors
    ///
    /// Returns an error if fewer than `length` bytes remain.
    fn take(&mut self, length: usize) -> anyhow::Result<&'bytes [u8]> {
        let taken = self
            .bytes
            .get(self.position..self.position + length)
            .context("unexpected end of the timestamp dictionary")?;
        self.position += length;
        Ok(taken)
    }

    /// # Returns
    ///
    /// The next little-endian `u64` on success.
    ///
    /// # Errors
    ///
    /// Forwards [`Self::take`]'s return values on failure.
    fn u64(&mut self) -> anyhow::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }

    /// # Returns
    ///
    /// The next `u64` as a length on success.
    ///
    /// # Errors
    ///
    /// Forwards [`Self::u64`]'s return values on failure.
    fn length(&mut self) -> anyhow::Result<usize> {
        Ok(usize::try_from(self.u64()?)?)
    }
}

/// Reads the timestamp patterns of a C++ single-file archive (`TimestampDictionaryReader::read`).
///
/// # Returns
///
/// The stored pattern texts on success.
///
/// # Errors
///
/// Returns an error if the archive cannot be read or its timestamp dictionary is malformed.
fn read_patterns(path: &Path) -> anyhow::Result<Vec<String>> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let metadata_size = usize::try_from(u32::from_le_bytes(
        bytes[METADATA_SECTION_SIZE_OFFSET..METADATA_SECTION_SIZE_OFFSET + 4].try_into()?,
    ))?;
    let metadata = zstd::stream::decode_all(&bytes[HEADER_SIZE..HEADER_SIZE + metadata_size])?;

    let mut packets = Reader {
        bytes: &metadata,
        position: 1,
    };
    for _ in 0..metadata[0] {
        let packet_type = packets.take(1)?[0];
        let size = usize::try_from(u32::from_le_bytes(packets.take(4)?.try_into()?))?;
        let payload = packets.take(size)?;
        if TIMESTAMP_DICTIONARY_PACKET_TYPE != packet_type {
            continue;
        }

        let mut dictionary = Reader {
            bytes: payload,
            position: 0,
        };
        for _ in 0..dictionary.u64()? {
            let key_length = dictionary.length()?;
            dictionary.take(key_length)?;
            let num_column_ids = dictionary.length()?;
            dictionary.take(4 * num_column_ids)?;
            if matches!(dictionary.u64()?, EPOCH_ENCODING | DOUBLE_EPOCH_ENCODING) {
                dictionary.take(16)?;
            }
        }
        let mut patterns = Vec::new();
        for _ in 0..dictionary.u64()? {
            dictionary.u64()?;
            let length = dictionary.length()?;
            patterns.push(String::from_utf8(dictionary.take(length)?.to_vec())?);
        }
        return Ok(patterns);
    }
    Ok(Vec::new())
}

#[test]
fn every_fixture_pattern_compiles() -> anyhow::Result<()> {
    let archives_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/archives");
    let mut patterns = BTreeSet::new();
    for entry in std::fs::read_dir(&archives_dir)? {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| "clps" == extension)
        {
            patterns.extend(read_patterns(&path)?);
        }
    }

    for pattern in &patterns {
        let compiled = TimestampPattern::parse(pattern)
            .with_context(|| format!("compile stored pattern `{pattern}`"))?;
        assert_eq!(compiled.as_str(), pattern);
        assert_eq!(
            compiled.is_numeric(),
            !pattern.starts_with('"'),
            "{pattern}"
        );
    }
    assert!(
        patterns.len() >= 20,
        "only {} distinct patterns were found: {patterns:?}",
        patterns.len()
    );
    Ok(())
}
