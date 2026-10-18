//! The timestamp index: indexed timestamp columns with their ranges, and the pattern table.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::OnceLock;

use clp_s_timestamp::TimestampPattern;

use crate::ClassifiedVersion;
use crate::CorruptionReason;
use crate::FormatError;
use crate::FormatLocation;
use crate::NodeId;
use crate::TimestampPatternId;
use crate::UnsupportedFeature;
use crate::bytes_reader::ByteReader;
use crate::metadata::PacketType;

/// The archive's timestamp dictionary: indexed timestamp columns and timestamp patterns.
#[derive(Debug, Default)]
pub struct TimestampIndex {
    entries: Vec<TimestampIndexEntry>,
    patterns: HashMap<TimestampPatternId, PatternSlot>,
}

impl TimestampIndex {
    /// # Returns
    ///
    /// The authoritative timestamp entry (entry 0 by writer convention), or `None` if the archive
    /// has no timestamp column.
    #[must_use]
    pub fn authoritative(&self) -> Option<&TimestampIndexEntry> {
        self.entries.first()
    }

    /// # Returns
    ///
    /// Every entry, in file order.
    #[must_use]
    pub fn entries(&self) -> &[TimestampIndexEntry] {
        &self.entries
    }

    /// Returns the pattern with id `id`. A pattern that failed to compile fails here, not at open
    /// (DV-32).
    ///
    /// # Returns
    ///
    /// The compiled pattern on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if no pattern has id `id`.
    /// * [`FormatError::UnsupportedFeature`] if the pattern with id `id` failed to compile.
    pub fn pattern(&self, id: TimestampPatternId) -> Result<&TimestampPattern, FormatError> {
        let location = FormatLocation::MetadataPacket {
            packet_type: PacketType::TIMESTAMP_DICTIONARY,
        };
        let slot = self.patterns.get(&id).ok_or_else(|| {
            FormatError::corrupt(location, CorruptionReason::UnknownTimestampPattern(id))
        })?;
        slot.compiled
            .get_or_init(|| {
                std::str::from_utf8(&slot.text)
                    .ok()
                    .and_then(|text| TimestampPattern::parse(text).ok())
            })
            .as_ref()
            .ok_or_else(|| FormatError::UnsupportedFeature {
                location,
                feature: UnsupportedFeature::TimestampPattern(id.get()),
            })
    }

    /// # Returns
    ///
    /// The ids of every stored pattern, in ascending order.
    #[must_use]
    pub fn pattern_ids(&self) -> Vec<TimestampPatternId> {
        let mut ids: Vec<_> = self.patterns.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// # Returns
    ///
    /// The stored text of pattern `id`, or `None` if no pattern has that id.
    #[must_use]
    pub fn pattern_text(&self, id: TimestampPatternId) -> Option<&[u8]> {
        self.patterns.get(&id).map(|slot| &*slot.text)
    }

    /// Factory function.
    ///
    /// Parses the timestamp-dictionary packet (`TimestampDictionaryReader.cpp`): a `u64` entry
    /// count, the entries, a `u64` pattern count and `(u64 id, u64 length, bytes)` patterns.
    /// Bytes after the patterns are ignored (DV-30); patterns compile on first use (DV-32).
    ///
    /// # Returns
    ///
    /// The timestamp index on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if a pattern id repeats.
    /// * Forwards [`ByteReader::read_u64`]'s return values on failure.
    /// * Forwards [`ByteReader::read_bytes`]'s return values on failure.
    /// * Forwards [`ByteReader::check_count`]'s return values on failure.
    /// * Forwards [`TimestampIndexEntry::parse`]'s return values on failure.
    pub(crate) fn parse(payload: &[u8], version: ClassifiedVersion) -> Result<Self, FormatError> {
        const MIN_ENTRY_SIZE: u64 = 3 * 8;
        const MIN_PATTERN_SIZE: u64 = 2 * 8;

        let mut reader = ByteReader::new(
            payload,
            FormatLocation::MetadataPacket {
                packet_type: PacketType::TIMESTAMP_DICTIONARY,
            },
        );
        let num_entries = reader.read_u64()?;
        let mut entries = Vec::with_capacity(reader.check_count(num_entries, MIN_ENTRY_SIZE)?);
        for _ in 0..num_entries {
            entries.push(TimestampIndexEntry::parse(&mut reader, version)?);
        }

        let num_patterns = reader.read_u64()?;
        let mut patterns =
            HashMap::with_capacity(reader.check_count(num_patterns, MIN_PATTERN_SIZE)?);
        for _ in 0..num_patterns {
            let id = TimestampPatternId::new(reader.read_u64()?);
            let length = reader.read_u64()?;
            let text = reader.read_bytes(length)?;
            match patterns.entry(id) {
                Entry::Occupied(_) => {
                    return Err(reader.corrupt(CorruptionReason::InvalidTimestampEntry));
                }
                Entry::Vacant(slot) => {
                    slot.insert(PatternSlot {
                        text: text.into(),
                        compiled: OnceLock::new(),
                    });
                }
            }
        }
        Ok(Self { entries, patterns })
    }
}

/// One indexed timestamp column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimestampIndexEntry {
    /// The stored key text; informational only and never tokenized.
    pub key: Box<[u8]>,

    /// The schema-tree node of the column (exactly one for 0.5.x archives).
    pub node_id: NodeId,

    /// The range of the column's values, or `None` for the "unknown" encoding.
    pub range: Option<TimestampRange>,
}

impl TimestampIndexEntry {
    /// Parses one entry (`TimestampEntry.cpp`): `u64` key length, key, `u64` id count, `i32`
    /// node ids, `u64` encoding, then the range for the epoch encodings.
    ///
    /// # Returns
    ///
    /// The entry on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if the entry does not have exactly one valid node id, its range
    ///   is inverted, or its encoding is unknown in a known patch release.
    /// * [`FormatError::UnsupportedFeature`] if the encoding is the floating-point epoch encoding,
    ///   or is unknown in a newer patch release.
    /// * Forwards [`ByteReader::read_u64`]'s return values on failure.
    /// * Forwards [`ByteReader::read_i32`]'s return values on failure.
    /// * Forwards [`ByteReader::read_i64`]'s return values on failure.
    /// * Forwards [`ByteReader::read_bytes`]'s return values on failure.
    fn parse(reader: &mut ByteReader<'_>, version: ClassifiedVersion) -> Result<Self, FormatError> {
        const UNKNOWN_ENCODING: u64 = 0;
        const EPOCH_ENCODING: u64 = 1;
        const DOUBLE_EPOCH_ENCODING: u64 = 2;

        let key_length = reader.read_u64()?;
        let key: Box<[u8]> = reader.read_bytes(key_length)?.into();
        if 1 != reader.read_u64()? {
            return Err(reader.corrupt(CorruptionReason::InvalidTimestampEntry));
        }
        let node_id = u32::try_from(reader.read_i32()?)
            .map_err(|_| reader.corrupt(CorruptionReason::InvalidTimestampEntry))?;

        let range = match reader.read_u64()? {
            UNKNOWN_ENCODING => None,
            EPOCH_ENCODING => {
                let begin = reader.read_i64()?;
                let end = reader.read_i64()?;
                if begin > end {
                    return Err(reader.corrupt(CorruptionReason::InvalidTimestampEntry));
                }
                Some(TimestampRange::EpochMillis { begin, end })
            }
            encoding @ DOUBLE_EPOCH_ENCODING => {
                return Err(FormatError::UnsupportedFeature {
                    location: reader.location(),
                    feature: UnsupportedFeature::TimestampEncoding(encoding),
                });
            }
            encoding if version.is_newer_patch => {
                return Err(FormatError::UnsupportedFeature {
                    location: reader.location(),
                    feature: UnsupportedFeature::TimestampEncoding(encoding),
                });
            }
            _ => return Err(reader.corrupt(CorruptionReason::InvalidTimestampEntry)),
        };
        Ok(Self {
            key,
            node_id: NodeId::new(node_id),
            range,
        })
    }
}

/// A timestamp range with an explicit unit.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimestampRange {
    /// Inclusive epoch-millisecond bounds (floor and ceiling of the nanosecond values).
    EpochMillis { begin: i64, end: i64 },
}

/// A stored pattern and its lazily compiled form (`None` if compilation failed).
#[derive(Debug)]
struct PatternSlot {
    text: Box<[u8]>,
    compiled: OnceLock<Option<TimestampPattern>>,
}

#[cfg(test)]
mod tests {
    use super::TimestampIndex;
    use super::TimestampRange;
    use crate::ClassifiedVersion;
    use crate::CorruptionReason;
    use crate::FormatError;
    use crate::FormatVersion;
    use crate::NodeId;
    use crate::TimestampPatternId;
    use crate::UnsupportedFeature;

    const VERSION_0_5_1: FormatVersion = FormatVersion {
        major: 0,
        minor: 5,
        patch: 1,
    };

    /// # Returns
    ///
    /// The classified version 0.5.`patch`.
    fn version(patch: u16) -> ClassifiedVersion {
        ClassifiedVersion::classify(FormatVersion {
            patch,
            ..VERSION_0_5_1
        })
        .expect("0.5.x is in the window")
    }

    /// # Returns
    ///
    /// One encoded timestamp-dictionary entry.
    fn encode_entry(key: &[u8], node_ids: &[i32], encoding: u64, range: &[i64]) -> Vec<u8> {
        let mut bytes = (key.len() as u64).to_le_bytes().to_vec();
        bytes.extend_from_slice(key);
        bytes.extend_from_slice(&(node_ids.len() as u64).to_le_bytes());
        for node_id in node_ids {
            bytes.extend_from_slice(&node_id.to_le_bytes());
        }
        bytes.extend_from_slice(&encoding.to_le_bytes());
        for bound in range {
            bytes.extend_from_slice(&bound.to_le_bytes());
        }
        bytes
    }

    /// # Returns
    ///
    /// A timestamp-dictionary payload of `entries` and `(id, text)` patterns.
    fn encode(entries: &[Vec<u8>], patterns: &[(u64, &[u8])]) -> Vec<u8> {
        let mut bytes = (entries.len() as u64).to_le_bytes().to_vec();
        for entry in entries {
            bytes.extend_from_slice(entry);
        }
        bytes.extend_from_slice(&(patterns.len() as u64).to_le_bytes());
        for (id, text) in patterns {
            bytes.extend_from_slice(&id.to_le_bytes());
            bytes.extend_from_slice(&(text.len() as u64).to_le_bytes());
            bytes.extend_from_slice(text);
        }
        bytes
    }

    /// # Returns
    ///
    /// The corruption reason of `error`.
    fn corruption_reason(error: FormatError) -> CorruptionReason {
        match error {
            FormatError::Corrupt { reason, .. } => *reason,
            other => panic!("expected a corruption error, got {other:?}"),
        }
    }

    #[test]
    fn parses_entries_and_patterns_and_ignores_trailing_bytes() -> anyhow::Result<()> {
        let mut payload = encode(
            &[
                encode_entry(b"ts", &[3], 1, &[-5, 1_700_000_000_000]),
                encode_entry(b"a\\.b", &[7], 0, &[]),
            ],
            &[(4, b"\\Y"), (9, b"\\s")],
        );
        payload.extend_from_slice(b"trailing");

        let index = TimestampIndex::parse(&payload, version(1))?;
        let authoritative = index
            .authoritative()
            .expect("the first entry is authoritative");
        assert_eq!(&*authoritative.key, b"ts");
        assert_eq!(authoritative.node_id, NodeId::new(3));
        assert_eq!(
            authoritative.range,
            Some(TimestampRange::EpochMillis {
                begin: -5,
                end: 1_700_000_000_000,
            })
        );
        assert_eq!(index.entries().len(), 2);
        assert_eq!(&*index.entries()[1].key, b"a\\.b");
        assert_eq!(index.entries()[1].range, None);
        assert_eq!(
            index.pattern_ids(),
            vec![TimestampPatternId::new(4), TimestampPatternId::new(9)]
        );
        assert_eq!(
            index.pattern_text(TimestampPatternId::new(9)),
            Some(&b"\\s"[..])
        );
        Ok(())
    }

    #[test]
    fn an_empty_dictionary_has_no_authoritative_entry() -> anyhow::Result<()> {
        let index = TimestampIndex::parse(&encode(&[], &[]), version(1))?;
        assert_eq!(index.authoritative(), None);
        assert_eq!(index.pattern_ids(), vec![]);
        Ok(())
    }

    #[test]
    fn unknown_and_unparsable_patterns_fail_only_on_access() -> anyhow::Result<()> {
        let index = TimestampIndex::parse(&encode(&[], &[(1, &[0xff, 0xfe])]), version(1))?;
        let error = index
            .pattern(TimestampPatternId::new(1))
            .expect_err("a non-UTF-8 pattern cannot compile");
        assert!(
            matches!(
                error,
                FormatError::UnsupportedFeature {
                    feature: UnsupportedFeature::TimestampPattern(1),
                    ..
                }
            ),
            "{error:?}"
        );

        let error = index
            .pattern(TimestampPatternId::new(2))
            .expect_err("pattern 2 does not exist");
        assert_eq!(
            corruption_reason(error),
            CorruptionReason::UnknownTimestampPattern(TimestampPatternId::new(2))
        );
        Ok(())
    }

    #[test]
    fn rejects_malformed_entries() {
        let cases = [
            encode(&[encode_entry(b"ts", &[], 1, &[0, 1])], &[]),
            encode(&[encode_entry(b"ts", &[1, 2], 1, &[0, 1])], &[]),
            encode(&[encode_entry(b"ts", &[-1], 1, &[0, 1])], &[]),
            encode(&[encode_entry(b"ts", &[1], 1, &[2, 1])], &[]),
            encode(&[encode_entry(b"ts", &[1], 3, &[])], &[]),
            encode(&[], &[(1, b"a"), (1, b"b")]),
        ];
        for payload in cases {
            let error = TimestampIndex::parse(&payload, version(1))
                .expect_err("a malformed timestamp dictionary must be rejected");
            assert_eq!(
                corruption_reason(error),
                CorruptionReason::InvalidTimestampEntry
            );
        }
    }

    #[test]
    fn unsupported_encodings_are_unsupported_features() {
        for (encoding, patch) in [(2, 1), (3, 2)] {
            let payload = encode(&[encode_entry(b"ts", &[1], encoding, &[0, 1])], &[]);
            let error = TimestampIndex::parse(&payload, version(patch))
                .expect_err("the encoding is not supported");
            assert!(
                matches!(
                    error,
                    FormatError::UnsupportedFeature {
                        feature: UnsupportedFeature::TimestampEncoding(rejected),
                        ..
                    } if rejected == encoding
                ),
                "{error:?}"
            );
        }
    }

    #[test]
    fn rejects_truncated_payloads_and_huge_counts() {
        let payload = encode(&[encode_entry(b"ts", &[3], 1, &[0, 1])], &[(4, b"\\Y")]);
        for length in 0..payload.len() {
            assert!(
                TimestampIndex::parse(&payload[..length], version(1)).is_err(),
                "a payload truncated to {length} bytes must be rejected"
            );
        }

        let error = TimestampIndex::parse(&u64::MAX.to_le_bytes(), version(1))
            .expect_err("the entry count cannot fit");
        assert!(
            matches!(
                corruption_reason(error),
                CorruptionReason::CountExceedsRemaining {
                    count: u64::MAX,
                    ..
                }
            ),
            "a huge count must be rejected before allocating"
        );
    }
}
