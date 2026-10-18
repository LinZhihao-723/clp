//! The range index: `log_event_idx` ranges with per-range metadata fields (e.g. `_filename`).

use std::ops::Range;

use crate::CorruptionReason;
use crate::FormatError;
use crate::FormatLocation;
use crate::SharedBytes;
use crate::bytes_reader::ByteReader;
use crate::metadata::PacketType;
use crate::msgpack;
use crate::msgpack::Item;

/// The archive's range index, validated: every entry has `begin <= end`.
#[derive(Debug)]
pub struct RangeIndex {
    entries: Vec<RangeIndexEntry>,

    /// Indices of the non-empty entries sorted by end, keeping the first entry of each end (the
    /// C++ `std::map::emplace` semantics).
    non_empty_by_end: Vec<usize>,
}

impl RangeIndex {
    const LOCATION: FormatLocation = FormatLocation::MetadataPacket {
        packet_type: PacketType::RANGE_INDEX,
    };

    /// # Returns
    ///
    /// Every entry, in file order.
    #[must_use]
    pub fn entries(&self) -> &[RangeIndexEntry] {
        &self.entries
    }

    /// # Returns
    ///
    /// The non-empty entry containing `log_event_idx` (C++ `upper_bound` semantics), or `None`.
    #[must_use]
    pub fn entry_containing(&self, log_event_idx: u64) -> Option<&RangeIndexEntry> {
        let position = self
            .non_empty_by_end
            .partition_point(|index| self.entries[*index].log_event_range.end <= log_event_idx);
        self.non_empty_by_end
            .get(position)
            .map(|index| &self.entries[*index])
    }

    /// Factory function.
    ///
    /// Parses the range-index packet (`ArchiveReaderAdaptor.cpp`): a msgpack array of maps with
    /// an unsigned start `s`, an unsigned end `e` and a map of metadata fields `f`. Bytes after
    /// the array are ignored.
    ///
    /// # Returns
    ///
    /// The range index on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if the payload is not an array of valid entries.
    /// * Forwards [`msgpack::read_item`]'s return values on failure.
    /// * Forwards [`msgpack::skip_children`]'s return values on failure.
    /// * Forwards [`MetadataFields::parse`]'s return values on failure.
    pub(crate) fn parse(payload: &SharedBytes) -> Result<Self, FormatError> {
        let mut reader = ByteReader::new(payload, Self::LOCATION);
        let invalid =
            |reader: &ByteReader<'_>| reader.corrupt(CorruptionReason::InvalidRangeIndexEntry);
        let Item::Array(num_entries) = msgpack::read_item(&mut reader)? else {
            return Err(invalid(&reader));
        };
        let mut entries = Vec::with_capacity(reader.check_count(num_entries.into(), 1)?);
        for _ in 0..num_entries {
            let Item::Map(num_fields) = msgpack::read_item(&mut reader)? else {
                return Err(invalid(&reader));
            };
            let mut begin = None;
            let mut end = None;
            let mut fields = None;
            for _ in 0..num_fields {
                let Item::String(key) = msgpack::read_item(&mut reader)? else {
                    return Err(invalid(&reader));
                };
                match key {
                    b"s" | b"e" => {
                        let Item::UnsignedInteger(value) = msgpack::read_item(&mut reader)? else {
                            return Err(invalid(&reader));
                        };
                        if b"s" == key {
                            begin = Some(value);
                        } else {
                            end = Some(value);
                        }
                    }
                    b"f" => fields = Some(MetadataFields::parse(&mut reader, payload)?),
                    _ => {
                        let value = msgpack::read_item(&mut reader)?;
                        msgpack::skip_children(&mut reader, value)?;
                    }
                }
            }
            let (Some(begin), Some(end), Some(fields)) = (begin, end, fields) else {
                return Err(invalid(&reader));
            };
            if begin > end {
                return Err(invalid(&reader));
            }
            entries.push(RangeIndexEntry {
                log_event_range: begin..end,
                fields,
            });
        }

        let mut non_empty_by_end: Vec<usize> = (0..entries.len())
            .filter(|index| !entries[*index].log_event_range.is_empty())
            .collect();
        non_empty_by_end.sort_by_key(|index| entries[*index].log_event_range.end);
        non_empty_by_end.dedup_by_key(|index| entries[*index].log_event_range.end);
        Ok(Self {
            entries,
            non_empty_by_end,
        })
    }
}

/// One range-index entry.
#[derive(Clone, Debug)]
pub struct RangeIndexEntry {
    pub log_event_range: Range<u64>,
    pub fields: MetadataFields,
}

/// One validated msgpack map, decoded on access.
#[derive(Clone, Debug)]
pub struct MetadataFields {
    encoded: SharedBytes,
}

impl MetadataFields {
    /// # Returns
    ///
    /// The value of the top-level field `key`, or `None` if absent.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<MetadataValue<'_>> {
        self.decode()
            .find(|(field_key, _)| *field_key == key)
            .map(|(_, value)| value)
    }

    /// # Returns
    ///
    /// The top-level fields in encoded order.
    #[must_use]
    pub fn to_vec(&self) -> Vec<(&str, MetadataValue<'_>)> {
        self.decode().collect()
    }

    /// # Returns
    ///
    /// The encoded map.
    #[must_use]
    pub fn encoded(&self) -> &[u8] {
        &self.encoded
    }

    /// Reads one map value from `reader`, a reader over `payload`, and validates it: string keys,
    /// and UTF-8 text in keys and top-level string values.
    ///
    /// # Returns
    ///
    /// The fields, sharing `payload`'s bytes, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if the value is not such a map.
    /// * Forwards [`msgpack::skip_value`]'s return values on failure.
    /// * Forwards [`msgpack::read_item`]'s return values on failure.
    /// * Forwards [`msgpack::skip_children`]'s return values on failure.
    fn parse(reader: &mut ByteReader<'_>, payload: &SharedBytes) -> Result<Self, FormatError> {
        let begin = reader.position();
        msgpack::skip_value(reader)?;
        let encoded = payload.slice(begin..reader.position());

        let mut field_reader = ByteReader::new(&encoded, reader.location());
        let invalid = || reader.corrupt(CorruptionReason::InvalidRangeIndexEntry);
        let Item::Map(num_fields) = msgpack::read_item(&mut field_reader)? else {
            return Err(invalid());
        };
        for _ in 0..num_fields {
            let Item::String(key) = msgpack::read_item(&mut field_reader)? else {
                return Err(invalid());
            };
            let value = msgpack::read_item(&mut field_reader)?;
            if let Item::String(text) = value
                && std::str::from_utf8(text).is_err()
            {
                return Err(reader.corrupt(CorruptionReason::InvalidUtf8));
            }
            if std::str::from_utf8(key).is_err() {
                return Err(reader.corrupt(CorruptionReason::InvalidUtf8));
            }
            msgpack::skip_children(&mut field_reader, value)?;
        }
        Ok(Self { encoded })
    }

    /// # Returns
    ///
    /// An iterator over the validated top-level fields.
    fn decode(&self) -> impl Iterator<Item = (&str, MetadataValue<'_>)> {
        let mut reader = ByteReader::new(&self.encoded, RangeIndex::LOCATION);
        let num_fields = match msgpack::read_item(&mut reader) {
            Ok(Item::Map(num_fields)) => num_fields,
            _ => 0,
        };
        let encoded: &[u8] = &self.encoded;
        (0..num_fields).map_while(move |_| {
            let Ok(Item::String(key)) = msgpack::read_item(&mut reader) else {
                return None;
            };
            let key = std::str::from_utf8(key).ok()?;
            let begin = reader.position();
            let item = msgpack::read_item(&mut reader).ok()?;
            msgpack::skip_children(&mut reader, item).ok()?;
            let value = match item {
                Item::Nil => MetadataValue::Null,
                Item::Boolean(value) => MetadataValue::Boolean(value),
                Item::UnsignedInteger(value) => MetadataValue::UnsignedInteger(value),
                Item::Integer(value) => MetadataValue::Integer(value),
                Item::Float(value) => MetadataValue::Float(value),
                Item::String(text) => MetadataValue::String(std::str::from_utf8(text).ok()?),
                Item::Binary(bytes) | Item::Extension(bytes) => MetadataValue::Binary(bytes),
                Item::Array(_) | Item::Map(_) => {
                    MetadataValue::Nested(&encoded[begin..reader.position()])
                }
            };
            Some((key, value))
        })
    }
}

/// A borrowed metadata value; unsigned integers stay exact (DV-41).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MetadataValue<'fields> {
    Null,
    Boolean(bool),
    Integer(i64),
    UnsignedInteger(u64),
    Float(f64),
    String(&'fields str),
    Binary(&'fields [u8]),

    /// A nested array or map, still msgpack-encoded.
    Nested(&'fields [u8]),
}

#[cfg(test)]
mod tests {
    use super::MetadataValue;
    use super::RangeIndex;
    use crate::CorruptionReason;
    use crate::FormatError;
    use crate::SharedBytes;

    /// # Returns
    ///
    /// `text` encoded as a msgpack `fixstr`.
    fn str_item(text: &str) -> Vec<u8> {
        let mut bytes = vec![0xa0 | u8::try_from(text.len()).expect("short test string")];
        bytes.extend_from_slice(text.as_bytes());
        bytes
    }

    /// # Returns
    ///
    /// `value` encoded as a msgpack `uint64`.
    fn uint_item(value: u64) -> Vec<u8> {
        let mut bytes = vec![0xcf];
        bytes.extend_from_slice(&value.to_be_bytes());
        bytes
    }

    /// # Returns
    ///
    /// A range-index entry `{s, e, f}` with the encoded `fields` map.
    fn entry(begin: u64, end: u64, fields: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0x83];
        bytes.extend(str_item("s"));
        bytes.extend(uint_item(begin));
        bytes.extend(str_item("e"));
        bytes.extend(uint_item(end));
        bytes.extend(str_item("f"));
        bytes.extend_from_slice(fields);
        bytes
    }

    /// # Returns
    ///
    /// The range-index payload holding `entries`.
    fn encode(entries: &[Vec<u8>]) -> SharedBytes {
        let mut bytes = vec![0x90 | u8::try_from(entries.len()).expect("few test entries")];
        for entry in entries {
            bytes.extend_from_slice(entry);
        }
        SharedBytes::from(bytes)
    }

    /// # Returns
    ///
    /// A metadata map with `_filename` set to `name` and a few other value kinds.
    fn filename_fields(name: &str) -> Vec<u8> {
        let mut bytes = vec![0x84];
        bytes.extend(str_item("_filename"));
        bytes.extend(str_item(name));
        bytes.extend(str_item("big"));
        bytes.extend(uint_item(u64::MAX));
        bytes.extend(str_item("neg"));
        bytes.push(0xff);
        bytes.extend(str_item("nested"));
        bytes.extend_from_slice(&[0x92, 0x01, 0xc0]);
        bytes
    }

    #[test]
    fn parses_entries_and_decodes_fields_on_access() -> anyhow::Result<()> {
        let index = RangeIndex::parse(&encode(&[
            entry(0, 10, &filename_fields("a.log")),
            entry(10, 10, &[0x80]),
            entry(10, 25, &filename_fields("b.log")),
        ]))?;
        assert_eq!(index.entries().len(), 3);
        assert_eq!(index.entries()[2].log_event_range, 10..25);

        let fields = &index.entries()[0].fields;
        assert_eq!(
            fields.get("_filename"),
            Some(MetadataValue::String("a.log"))
        );
        assert_eq!(
            fields.get("big"),
            Some(MetadataValue::UnsignedInteger(u64::MAX))
        );
        assert_eq!(fields.get("neg"), Some(MetadataValue::Integer(-1)));
        assert_eq!(
            fields.get("nested"),
            Some(MetadataValue::Nested(&[0x92, 0x01, 0xc0]))
        );
        assert_eq!(fields.get("missing"), None);
        assert_eq!(
            fields
                .to_vec()
                .into_iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>(),
            vec!["_filename", "big", "neg", "nested"]
        );
        assert_eq!(index.entries()[1].fields.to_vec(), vec![]);
        Ok(())
    }

    #[test]
    fn entry_lookup_follows_cpp_upper_bound_over_non_empty_ranges() -> anyhow::Result<()> {
        let index = RangeIndex::parse(&encode(&[
            entry(0, 10, &filename_fields("a")),
            entry(10, 10, &filename_fields("empty")),
            entry(10, 25, &filename_fields("b")),
        ]))?;
        let name_at = |log_event_idx| {
            index
                .entry_containing(log_event_idx)
                .and_then(|entry| entry.fields.get("_filename"))
        };
        assert_eq!(name_at(0), Some(MetadataValue::String("a")));
        assert_eq!(name_at(9), Some(MetadataValue::String("a")));
        assert_eq!(name_at(10), Some(MetadataValue::String("b")));
        assert_eq!(name_at(24), Some(MetadataValue::String("b")));
        assert_eq!(name_at(25), None);
        Ok(())
    }

    #[test]
    fn empty_ranges_are_never_found_across_a_gap() -> anyhow::Result<()> {
        let index = RangeIndex::parse(&encode(&[
            entry(0, 5, &filename_fields("a")),
            entry(7, 7, &filename_fields("empty")),
            entry(9, 12, &filename_fields("c")),
        ]))?;
        let name_at = |log_event_idx| {
            index
                .entry_containing(log_event_idx)
                .and_then(|entry| entry.fields.get("_filename"))
        };
        assert_eq!(name_at(4), Some(MetadataValue::String("a")));
        assert_eq!(name_at(6), Some(MetadataValue::String("c")));
        assert_eq!(name_at(7), Some(MetadataValue::String("c")));
        Ok(())
    }

    #[test]
    fn rejects_invalid_entries() {
        let mut signed_begin = vec![0x83];
        signed_begin.extend(str_item("s"));
        signed_begin.push(0xff);
        signed_begin.extend(str_item("e"));
        signed_begin.push(0x01);
        signed_begin.extend(str_item("f"));
        signed_begin.push(0x80);

        let cases = [
            SharedBytes::from_static(&[0x80]),
            encode(&[entry(5, 4, &[0x80])]),
            encode(&[entry(0, 1, &[0x90])]),
            encode(&[signed_begin]),
            encode(&[vec![0x81, 0xa1, b's', 0x00]]),
        ];
        for payload in cases {
            let error = RangeIndex::parse(&payload).expect_err("the entry is invalid");
            assert!(
                matches!(
                    &error,
                    FormatError::Corrupt { reason, .. }
                        if **reason == CorruptionReason::InvalidRangeIndexEntry
                ),
                "{payload:02x?}: {error:?}"
            );
        }

        let mut bad_utf8 = vec![0x81];
        bad_utf8.extend(str_item("k"));
        bad_utf8.extend_from_slice(&[0xa1, 0xff]);
        let error = RangeIndex::parse(&encode(&[entry(0, 1, &bad_utf8)]))
            .expect_err("field strings must be UTF-8");
        assert!(
            matches!(
                &error,
                FormatError::Corrupt { reason, .. } if **reason == CorruptionReason::InvalidUtf8
            ),
            "{error:?}"
        );
    }

    #[test]
    fn rejects_every_truncation() -> anyhow::Result<()> {
        let payload = encode(&[entry(0, 10, &filename_fields("a.log"))]);
        RangeIndex::parse(&payload)?;
        for length in 0..payload.len() {
            assert!(
                RangeIndex::parse(&payload.slice(..length)).is_err(),
                "a payload truncated to {length} bytes must be rejected"
            );
        }
        Ok(())
    }
}
