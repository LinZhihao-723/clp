//! The variable, logtype and array dictionaries.

use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;
use std::sync::OnceLock;

use clp_s_encoding::VariablePlaceholder;

use crate::CorruptionReason;
use crate::FormatError;
use crate::FormatLocation;
use crate::LogTypeId;
use crate::ReaderLimits;
use crate::SectionId;
use crate::SharedBytes;
use crate::VariableId;
use crate::bytes_reader::ByteReader;
use crate::decode;

/// One of the three dictionaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DictionaryKind {
    Variable,
    LogType,
    Array,
}

/// The variable dictionary (`VarString`, `DictionaryFloat` and `ClpString` dictionary variables),
/// as one values buffer plus offsets.
#[derive(Debug)]
pub struct VariableDictionary {
    entries: Entries,
}

impl VariableDictionary {
    /// Factory function.
    ///
    /// Decodes a dictionary section: a raw little-endian `u64` entry count followed by one zstd
    /// frame of `(u64 length, bytes)` records.
    ///
    /// # Returns
    ///
    /// The decoded dictionary on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if the section is malformed.
    /// * [`FormatError::LimitExceeded`] if the entry count or decoded size exceeds `limits`.
    pub fn load(section: &[u8], limits: &ReaderLimits) -> Result<Self, FormatError> {
        Ok(Self {
            entries: Entries::load(section, SectionId::VariableDictionary, limits)?,
        })
    }

    /// # Returns
    ///
    /// The number of entries.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.entries.len()
    }

    /// # Returns
    ///
    /// Whether the dictionary has no entry.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.spans.is_empty()
    }

    /// Returns entry `id`; ids read from columns are untrusted, so this is checked.
    ///
    /// # Returns
    ///
    /// The entry bytes on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if `id` is out of range.
    pub fn get(&self, id: VariableId) -> Result<&[u8], FormatError> {
        self.entries.get(id.get())
    }

    /// Resolves exact probes in one batched pass; ASCII case is folded unless
    /// `is_case_sensitive`, so one probe can match several entries.
    ///
    /// # Returns
    ///
    /// The ids of the matching entries of every probe that matched at least one entry, in
    /// ascending id order.
    #[must_use]
    pub fn find_exact<'probe>(
        &self,
        probes: &[&'probe [u8]],
        is_case_sensitive: bool,
    ) -> HashMap<&'probe [u8], Box<[VariableId]>> {
        let mut matches: HashMap<&'probe [u8], Vec<VariableId>> = HashMap::new();
        if is_case_sensitive {
            let probe_set: HashSet<&[u8]> = probes.iter().copied().collect();
            for (id, entry) in self.entries.iter() {
                if let Some(probe) = probe_set.get(entry) {
                    matches.entry(probe).or_default().push(VariableId::new(id));
                }
            }
        } else {
            let mut probes_by_folded: HashMap<Vec<u8>, Vec<&'probe [u8]>> = HashMap::new();
            for probe in probes {
                let folded = probe.to_ascii_lowercase();
                let folded_probes = probes_by_folded.entry(folded).or_default();
                if !folded_probes.contains(probe) {
                    folded_probes.push(probe);
                }
            }
            let mut folded_entry = Vec::new();
            for (id, entry) in self.entries.iter() {
                folded_entry.clear();
                folded_entry.extend(entry.iter().map(u8::to_ascii_lowercase));
                let Some(folded_probes) = probes_by_folded.get(folded_entry.as_slice()) else {
                    continue;
                };
                for probe in folded_probes {
                    matches.entry(probe).or_default().push(VariableId::new(id));
                }
            }
        }
        matches
            .into_iter()
            .map(|(probe, ids)| (probe, ids.into_boxed_slice()))
            .collect()
    }

    /// Calls `visit` with every entry in id order, stopping early if it returns `false`.
    ///
    /// # Type Parameters
    ///
    /// * `VisitorType` - Receives each `(id, entry)`; returns whether to continue.
    pub fn for_each_entry<VisitorType: FnMut(VariableId, &[u8]) -> bool>(
        &self,
        mut visit: VisitorType,
    ) {
        for (id, entry) in self.entries.iter() {
            if !visit(VariableId::new(id), entry) {
                break;
            }
        }
    }
}

/// A logtype or array dictionary, with the placeholders of every entry indexed at load time.
#[derive(Debug)]
pub struct LogTypeDictionary {
    entries: Entries,
    placeholders: Vec<LogtypePlaceholder>,

    /// `placeholders[placeholder_offsets[i]..placeholder_offsets[i + 1]]` belong to entry `i`.
    placeholder_offsets: Vec<usize>,
}

impl LogTypeDictionary {
    /// Factory function.
    ///
    /// Decodes a dictionary section (same framing as [`VariableDictionary::load`]) and indexes
    /// the placeholders of every entry.
    ///
    /// # Returns
    ///
    /// The decoded dictionary on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if the section is malformed.
    /// * [`FormatError::LimitExceeded`] if the entry count or decoded size exceeds `limits`.
    pub fn load(section: &[u8], limits: &ReaderLimits) -> Result<Self, FormatError> {
        Self::load_section(section, SectionId::LogTypeDictionary, limits)
    }

    /// Factory function.
    ///
    /// Decodes the array dictionary section; the same as [`Self::load`], with errors located in
    /// `/array.dict`.
    ///
    /// # Returns
    ///
    /// The decoded dictionary on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if the section is malformed.
    /// * [`FormatError::LimitExceeded`] if the entry count or decoded size exceeds `limits`.
    pub fn load_array(section: &[u8], limits: &ReaderLimits) -> Result<Self, FormatError> {
        Self::load_section(section, SectionId::ArrayDictionary, limits)
    }

    /// # Returns
    ///
    /// The number of entries.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.entries.len()
    }

    /// # Returns
    ///
    /// Whether the dictionary has no entry.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.spans.is_empty()
    }

    /// Returns entry `id`; ids read from columns are untrusted, so this is checked.
    ///
    /// # Returns
    ///
    /// The logtype on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if `id` is out of range.
    pub fn get(&self, id: LogTypeId) -> Result<Logtype<'_>, FormatError> {
        let text = self.entries.get(id.get())?;
        Ok(self.logtype(usize::try_from(id.get()).unwrap_or(usize::MAX), text))
    }

    /// Calls `visit` with every entry in id order, stopping early if it returns `false`.
    ///
    /// # Type Parameters
    ///
    /// * `VisitorType` - Receives each `(id, entry)`; returns whether to continue.
    pub fn for_each_entry<VisitorType: FnMut(LogTypeId, Logtype<'_>) -> bool>(
        &self,
        mut visit: VisitorType,
    ) {
        for (index, (id, text)) in self.entries.iter().enumerate() {
            if !visit(LogTypeId::new(id), self.logtype(index, text)) {
                break;
            }
        }
    }

    /// Decodes a logtype or array dictionary section and indexes the placeholders of every entry
    /// (`LogTypeDictionaryEntry::decode_log_type`): an escape byte is a placeholder and makes the
    /// next byte literal; every other placeholder byte is a variable placeholder.
    ///
    /// # Returns
    ///
    /// The decoded dictionary on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::LimitExceeded`] if an entry is longer than `u32::MAX` bytes.
    /// * Forwards [`Entries::load`]'s return values on failure.
    fn load_section(
        section: &[u8],
        section_id: SectionId,
        limits: &ReaderLimits,
    ) -> Result<Self, FormatError> {
        let entries = Entries::load(section, section_id, limits)?;
        let mut placeholders = Vec::new();
        let mut placeholder_offsets = Vec::with_capacity(entries.spans.len() + 1);
        placeholder_offsets.push(0);
        for (_, text) in entries.iter() {
            FormatError::check_limit("logtype length", text.len() as u64, u32::MAX.into())?;
            let mut is_escaped = false;
            for (offset, byte) in (0_u32..).zip(text.iter().copied()) {
                if is_escaped {
                    is_escaped = false;
                    continue;
                }
                let Some(kind) = VariablePlaceholder::from_byte(byte) else {
                    continue;
                };
                is_escaped = VariablePlaceholder::Escape == kind;
                placeholders.push(LogtypePlaceholder { offset, kind });
            }
            placeholder_offsets.push(placeholders.len());
        }
        Ok(Self {
            entries,
            placeholders,
            placeholder_offsets,
        })
    }

    /// # Returns
    ///
    /// The logtype of entry `index`, whose text is `text`.
    fn logtype<'dictionary>(
        &'dictionary self,
        index: usize,
        text: &'dictionary [u8],
    ) -> Logtype<'dictionary> {
        Logtype {
            text,
            placeholders: &self.placeholders
                [self.placeholder_offsets[index]..self.placeholder_offsets[index + 1]],
        }
    }
}

/// A borrowed logtype: its text (with placeholder bytes) and its indexed placeholders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Logtype<'dictionary> {
    /// The logtype text, including placeholder and escape bytes.
    pub text: &'dictionary [u8],

    /// The placeholders of the text, in order.
    pub placeholders: &'dictionary [LogtypePlaceholder],
}

/// One placeholder of a logtype.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogtypePlaceholder {
    /// Byte offset of the placeholder in the logtype text.
    pub offset: u32,

    pub kind: VariablePlaceholder,
}

/// The three dictionaries of one archive, each loaded at most once.
///
/// Storage fills the cells; the table and search layers read them. A missing dictionary is a
/// driver bug that callers report as an internal error.
#[derive(Debug, Default)]
pub struct ArchiveDictionaries {
    pub variable: OnceLock<Arc<VariableDictionary>>,
    pub logtype: OnceLock<Arc<LogTypeDictionary>>,
    pub array: OnceLock<Arc<LogTypeDictionary>>,
}

/// The entries of a dictionary section: the decoded frame plus the span of every entry in it.
#[derive(Debug)]
struct Entries {
    section_id: SectionId,
    decoded: SharedBytes,
    spans: Vec<Range<usize>>,
}

impl Entries {
    /// Decodes a dictionary section (`DictionaryReader.hpp`): a raw little-endian `u64` entry
    /// count followed by one zstd frame of `(u64 length, bytes)` records.
    ///
    /// # Returns
    ///
    /// The entries on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if the section is shorter than its count, or the frame holds
    ///   fewer entries than the count.
    /// * [`FormatError::LimitExceeded`] if the count exceeds
    ///   [`ReaderLimits::max_dictionary_entries`].
    /// * Forwards [`ByteReader::read_u64`]'s return values on failure.
    /// * Forwards [`ByteReader::read_bytes`]'s return values on failure.
    /// * Forwards [`ByteReader::check_count`]'s return values on failure.
    /// * Forwards [`decode::decode_section_frame`]'s return values on failure.
    fn load(
        section: &[u8],
        section_id: SectionId,
        limits: &ReaderLimits,
    ) -> Result<Self, FormatError> {
        const LENGTH_SIZE: u64 = 8;

        let location = FormatLocation::Section(section_id);
        let mut section_reader = ByteReader::new(section, location);
        let num_entries = section_reader.read_u64()?;
        FormatError::check_limit(
            "dictionary entry count",
            num_entries,
            limits.max_dictionary_entries,
        )?;
        let frame = &section[section_reader.position()..];
        let decoded = if frame.is_empty() && 0 == num_entries {
            SharedBytes::new()
        } else {
            decode::decode_section_frame(frame, location, limits)?
        };

        let mut reader = ByteReader::new(&decoded, location);
        let num_entries = reader.check_count(num_entries, LENGTH_SIZE)?;
        let mut spans = Vec::with_capacity(num_entries);
        for _ in 0..num_entries {
            let length = reader.read_u64()?;
            let begin = reader.position();
            reader.read_bytes(length)?;
            spans.push(begin..reader.position());
        }
        Ok(Self {
            section_id,
            decoded,
            spans,
        })
    }

    /// # Returns
    ///
    /// The number of entries.
    const fn len(&self) -> u64 {
        self.spans.len() as u64
    }

    /// # Returns
    ///
    /// Entry `id` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if `id` is out of range.
    fn get(&self, id: u64) -> Result<&[u8], FormatError> {
        usize::try_from(id)
            .ok()
            .and_then(|index| self.spans.get(index))
            .map(|span| &self.decoded[span.clone()])
            .ok_or_else(|| {
                FormatError::corrupt(
                    FormatLocation::Section(self.section_id),
                    CorruptionReason::IdOutOfRange {
                        id,
                        len: self.len(),
                    },
                )
            })
    }

    /// # Returns
    ///
    /// An iterator over `(id, entry)` in id order.
    fn iter(&self) -> impl Iterator<Item = (u64, &[u8])> {
        (0_u64..)
            .zip(&self.spans)
            .map(|(id, span)| (id, &self.decoded[span.clone()]))
    }
}

#[cfg(test)]
mod tests {
    use clp_s_encoding::VariablePlaceholder;

    use super::LogTypeDictionary;
    use super::LogtypePlaceholder;
    use super::VariableDictionary;
    use crate::CorruptionReason;
    use crate::FormatError;
    use crate::FormatLocation;
    use crate::LogTypeId;
    use crate::ReaderLimits;
    use crate::SectionId;
    use crate::VariableId;

    /// # Returns
    ///
    /// A dictionary section with the raw count `num_entries` and the frame of `entries`.
    fn encode_section(num_entries: u64, entries: &[&[u8]]) -> Vec<u8> {
        let mut records = Vec::new();
        for entry in entries {
            records.extend_from_slice(&(entry.len() as u64).to_le_bytes());
            records.extend_from_slice(entry);
        }
        let mut section = num_entries.to_le_bytes().to_vec();
        section.extend(zstd::encode_all(records.as_slice(), 3).expect("zstd compresses"));
        section
    }

    /// # Returns
    ///
    /// The location and reason of the corruption error `error`.
    fn corruption(error: FormatError) -> (FormatLocation, CorruptionReason) {
        match error {
            FormatError::Corrupt { location, reason } => (location, *reason),
            other => panic!("expected a corruption error, got {other:?}"),
        }
    }

    #[test]
    fn variable_entries_are_checked_by_id() -> anyhow::Result<()> {
        let entries: [&[u8]; 4] = [b"alpha", b"", b"Error", b"ERROR"];
        let dictionary = VariableDictionary::load(
            &encode_section(entries.len() as u64, &entries),
            &ReaderLimits::default(),
        )?;
        assert_eq!(dictionary.len(), 4);
        assert!(!dictionary.is_empty());
        for (id, entry) in (0_u64..).zip(entries) {
            assert_eq!(dictionary.get(VariableId::new(id))?, entry);
        }
        let (location, reason) = corruption(
            dictionary
                .get(VariableId::new(4))
                .expect_err("id 4 is out of range"),
        );
        assert_eq!(
            location,
            FormatLocation::Section(SectionId::VariableDictionary)
        );
        assert_eq!(reason, CorruptionReason::IdOutOfRange { id: 4, len: 4 });

        let mut visited = Vec::new();
        dictionary.for_each_entry(|id, entry| {
            visited.push((id, entry.to_vec()));
            id.get() < 1
        });
        assert_eq!(
            visited,
            vec![
                (VariableId::new(0), b"alpha".to_vec()),
                (VariableId::new(1), Vec::new())
            ]
        );
        Ok(())
    }

    #[test]
    fn exact_lookup_folds_ascii_case_only_when_asked() -> anyhow::Result<()> {
        let entries: [&[u8]; 5] = [
            b"error",
            b"Error",
            b"ERROR",
            b"warn",
            "\u{e9}rror".as_bytes(),
        ];
        let dictionary = VariableDictionary::load(
            &encode_section(entries.len() as u64, &entries),
            &ReaderLimits::default(),
        )?;
        let probes: [&[u8]; 4] = [b"Error", b"missing", b"WARN", "\u{c9}RROR".as_bytes()];

        let sensitive = dictionary.find_exact(&probes, true);
        assert_eq!(sensitive.len(), 1);
        assert_eq!(
            sensitive.get(&b"Error"[..]).map(|ids| &**ids),
            Some(&[VariableId::new(1)][..])
        );

        let insensitive = dictionary.find_exact(&probes, false);
        assert_eq!(insensitive.len(), 2);
        assert_eq!(
            insensitive.get(&b"Error"[..]).map(|ids| &**ids),
            Some([0, 1, 2].map(VariableId::new).as_slice())
        );
        assert_eq!(
            insensitive.get(&b"WARN"[..]).map(|ids| &**ids),
            Some(&[VariableId::new(3)][..])
        );
        assert_eq!(insensitive.get(&b"missing"[..]), None);
        assert_eq!(insensitive.get("\u{c9}RROR".as_bytes()), None);
        Ok(())
    }

    #[test]
    fn logtype_placeholders_are_indexed_at_load() -> anyhow::Result<()> {
        let entries: [&[u8]; 4] = [
            b"took \x11 ms at \x13 for \x12",
            b"escaped \\\x11 and \\\\ then \x12",
            b"plain",
            b"trailing \\",
        ];
        let dictionary = LogTypeDictionary::load(
            &encode_section(entries.len() as u64, &entries),
            &ReaderLimits::default(),
        )?;
        assert_eq!(dictionary.len(), 4);
        let placeholder = |offset, kind| LogtypePlaceholder { offset, kind };

        let logtype = dictionary.get(LogTypeId::new(0))?;
        assert_eq!(logtype.text, entries[0]);
        assert_eq!(
            logtype.placeholders,
            [
                placeholder(5, VariablePlaceholder::Integer),
                placeholder(13, VariablePlaceholder::Float),
                placeholder(19, VariablePlaceholder::Dictionary),
            ]
        );
        assert_eq!(
            dictionary.get(LogTypeId::new(1))?.placeholders,
            [
                placeholder(8, VariablePlaceholder::Escape),
                placeholder(15, VariablePlaceholder::Escape),
                placeholder(23, VariablePlaceholder::Dictionary),
            ]
        );
        assert_eq!(dictionary.get(LogTypeId::new(2))?.placeholders, []);
        assert_eq!(
            dictionary.get(LogTypeId::new(3))?.placeholders,
            [placeholder(9, VariablePlaceholder::Escape)]
        );

        let mut num_visited = 0;
        dictionary.for_each_entry(|id, logtype| {
            assert_eq!(logtype, dictionary.get(id).expect("visited ids exist"));
            num_visited += 1;
            true
        });
        assert_eq!(num_visited, 4);
        Ok(())
    }

    #[test]
    fn array_dictionary_errors_name_the_array_section() -> anyhow::Result<()> {
        let dictionary =
            LogTypeDictionary::load_array(&encode_section(1, &[b"[1]"]), &ReaderLimits::default())?;
        let (location, reason) = corruption(
            dictionary
                .get(LogTypeId::new(1))
                .expect_err("id 1 is out of range"),
        );
        assert_eq!(
            location,
            FormatLocation::Section(SectionId::ArrayDictionary)
        );
        assert_eq!(reason, CorruptionReason::IdOutOfRange { id: 1, len: 1 });
        Ok(())
    }

    #[test]
    fn empty_dictionaries_load_with_or_without_a_frame() -> anyhow::Result<()> {
        for section in [encode_section(0, &[]), 0_u64.to_le_bytes().to_vec()] {
            let dictionary = VariableDictionary::load(&section, &ReaderLimits::default())?;
            assert!(dictionary.is_empty(), "{dictionary:?}");
            assert_eq!(dictionary.len(), 0, "{dictionary:?}");
        }
        Ok(())
    }

    #[test]
    fn rejects_malformed_sections() {
        let limits = ReaderLimits::default();
        let mut trailing = encode_section(1, &[b"a"]);
        trailing.push(0);
        let cases = [
            (vec![1, 2, 3], "a section shorter than its count"),
            (
                encode_section(3, &[b"a", b"b"]),
                "fewer entries than the count",
            ),
            (encode_section(1000, &[b"a"]), "a count that cannot fit"),
            (trailing, "bytes after the frame"),
            (1_u64.to_le_bytes().to_vec(), "a missing frame"),
        ];
        for (section, description) in cases {
            let error = VariableDictionary::load(&section, &limits).expect_err(description);
            assert!(
                matches!(error, FormatError::Corrupt { .. }),
                "{description}: {error:?}"
            );
        }

        let limited = ReaderLimits {
            max_dictionary_entries: 1,
            ..limits
        };
        let error = LogTypeDictionary::load(&encode_section(2, &[b"a", b"b"]), &limited)
            .expect_err("two entries exceed the limit");
        assert!(
            matches!(
                error,
                FormatError::LimitExceeded {
                    actual_size: 2,
                    limit_size: 1,
                    ..
                }
            ),
            "{error:?}"
        );
    }
}
