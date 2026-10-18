//! Read planning: which tables and dictionaries a scan needs, and the streams to decode.

use std::collections::HashMap;

use crate::ByteRange;
use crate::DictionaryKind;
use crate::PackedStreamInfo;
use crate::TableOrdinal;

/// Dense bitset over the table directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableSet {
    words: Vec<u64>,
    num_tables: u32,
}

impl TableSet {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// An empty set over a directory of `num_tables` tables.
    #[must_use]
    pub fn empty(num_tables: u32) -> Self {
        Self {
            words: vec![0; num_tables.div_ceil(u64::BITS) as usize],
            num_tables,
        }
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The set of every table of a directory of `num_tables` tables.
    #[must_use]
    pub fn full(num_tables: u32) -> Self {
        let mut set = Self::empty(num_tables);
        set.words.fill(u64::MAX);
        let num_tail_bits = num_tables % u64::BITS;
        if let Some(last) = set.words.last_mut()
            && 0 != num_tail_bits
        {
            *last = (1_u64 << num_tail_bits) - 1;
        }
        set
    }

    /// Adds `table` to the set.
    ///
    /// # Panics
    ///
    /// Panics if `table` is outside the directory the set was created for.
    pub fn insert(&mut self, table: TableOrdinal) {
        assert!(
            table.get() < self.num_tables,
            "table {} is outside a directory of {} tables",
            table.get(),
            self.num_tables
        );
        self.words[(table.get() / u64::BITS) as usize] |= 1_u64 << (table.get() % u64::BITS);
    }

    /// # Returns
    ///
    /// Whether `table` is in the set.
    #[must_use]
    pub fn contains(&self, table: TableOrdinal) -> bool {
        table.get() < self.num_tables
            && 0 != self.words[(table.get() / u64::BITS) as usize]
                & (1_u64 << (table.get() % u64::BITS))
    }

    /// # Returns
    ///
    /// The number of tables in the set.
    #[must_use]
    pub fn len(&self) -> u32 {
        self.words.iter().map(|word| word.count_ones()).sum()
    }

    /// # Returns
    ///
    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|word| 0 == *word)
    }

    /// # Returns
    ///
    /// The tables of the set in ascending ordinal order.
    #[must_use]
    pub fn to_vec(&self) -> Vec<TableOrdinal> {
        let mut tables = Vec::with_capacity(self.len() as usize);
        for (word_index, word) in (0_u32..).zip(&self.words) {
            let mut remaining_bits = *word;
            while 0 != remaining_bits {
                let bit = remaining_bits.trailing_zeros();
                tables.push(TableOrdinal::new(word_index * u64::BITS + bit));
                remaining_bits &= remaining_bits - 1;
            }
        }
        tables
    }

    /// # Returns
    ///
    /// The number of tables of the directory the set was created for.
    #[must_use]
    pub const fn num_tables(&self) -> u32 {
        self.num_tables
    }
}

/// Which dictionaries a scan needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DictionaryNeeds {
    pub is_variable_needed: bool,
    pub is_logtype_needed: bool,
    pub is_array_needed: bool,
}

impl DictionaryNeeds {
    /// # Returns
    ///
    /// The union of `self` and `other`.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self {
            is_variable_needed: self.is_variable_needed || other.is_variable_needed,
            is_logtype_needed: self.is_logtype_needed || other.is_logtype_needed,
            is_array_needed: self.is_array_needed || other.is_array_needed,
        }
    }
}

/// Storage work for one scan of one archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadPlan {
    /// The absolute ranges of the dictionary sections to load.
    pub dictionary_ranges: HashMap<DictionaryKind, ByteRange>,

    /// The streams to decode, in ascending stream order.
    pub streams: Vec<StreamRead>,
}

/// One packed stream to fetch, and how far to decode it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamRead {
    pub stream: PackedStreamInfo,

    /// End of the last needed ERT in the decoded stream; decoding stops here.
    pub decoded_prefix_size: u64,

    /// The needed ERTs of this stream, in stream-offset order.
    pub tables: Vec<TableOrdinal>,
}

#[cfg(test)]
mod tests {
    use super::DictionaryNeeds;
    use super::TableSet;
    use crate::TableOrdinal;

    #[test]
    fn sets_track_members_across_word_boundaries() {
        let mut set = TableSet::empty(130);
        assert!(set.is_empty(), "{set:?}");
        assert_eq!(set.to_vec(), Vec::new());
        for ordinal in [0, 63, 64, 129] {
            set.insert(TableOrdinal::new(ordinal));
        }
        set.insert(TableOrdinal::new(64));
        assert_eq!(set.len(), 4);
        assert!(set.contains(TableOrdinal::new(63)));
        assert!(!set.contains(TableOrdinal::new(62)));
        assert!(!set.contains(TableOrdinal::new(130)));
        assert!(!set.contains(TableOrdinal::new(u32::MAX)));
        assert_eq!(
            set.to_vec(),
            [0, 63, 64, 129].map(TableOrdinal::new).to_vec()
        );
    }

    #[test]
    fn full_sets_keep_tail_bits_clear() {
        for num_tables in [0, 1, 63, 64, 65, 128] {
            let set = TableSet::full(num_tables);
            assert_eq!(set.len(), num_tables);
            assert_eq!(set.is_empty(), 0 == num_tables);
            assert_eq!(
                set.to_vec(),
                (0..num_tables).map(TableOrdinal::new).collect::<Vec<_>>()
            );
            assert!(!set.contains(TableOrdinal::new(num_tables)));
        }
        assert_eq!(TableSet::full(3), {
            let mut set = TableSet::empty(3);
            (0..3).for_each(|ordinal| set.insert(TableOrdinal::new(ordinal)));
            set
        });
    }

    #[test]
    #[should_panic(expected = "outside a directory")]
    fn inserting_outside_the_directory_panics() {
        TableSet::empty(3).insert(TableOrdinal::new(3));
    }

    #[test]
    fn dictionary_needs_union() {
        let variable = DictionaryNeeds {
            is_variable_needed: true,
            ..DictionaryNeeds::default()
        };
        let array = DictionaryNeeds {
            is_array_needed: true,
            ..DictionaryNeeds::default()
        };
        assert_eq!(
            variable.union(array),
            DictionaryNeeds {
                is_variable_needed: true,
                is_logtype_needed: false,
                is_array_needed: true,
            }
        );
    }
}
