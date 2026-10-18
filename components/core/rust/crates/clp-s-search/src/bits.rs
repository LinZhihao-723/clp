//! Dense bitsets over the table ordinals of one archive.

use clp_s_format::TableOrdinal;

/// A dense set of table ordinals, `u64` words with the tail bits clear.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableBits {
    words: Box<[u64]>,
    num_tables: u32,
}

impl TableBits {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The empty set over `num_tables` tables.
    pub fn empty(num_tables: u32) -> Self {
        Self {
            words: vec![0; num_tables.div_ceil(u64::BITS) as usize].into_boxed_slice(),
            num_tables,
        }
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The set of every one of `num_tables` tables.
    pub fn full(num_tables: u32) -> Self {
        let mut set = Self::empty(num_tables);
        set.words.fill(u64::MAX);
        set.mask_tail();
        set
    }

    /// Adds `table`; ordinals outside the set's range are ignored, since ordinals come from the
    /// validated table directory of the same archive.
    pub fn insert(&mut self, table: TableOrdinal) {
        let index = table.get();
        if index < self.num_tables {
            self.words[(index / u64::BITS) as usize] |= 1 << (index % u64::BITS);
        }
    }

    /// # Returns
    ///
    /// Whether `table` is in the set.
    pub fn contains(&self, table: TableOrdinal) -> bool {
        let index = table.get();
        index < self.num_tables
            && 0 != self.words[(index / u64::BITS) as usize] & (1 << (index % u64::BITS))
    }

    /// # Returns
    ///
    /// Whether the set has no table.
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|word| 0 == *word)
    }

    /// # Returns
    ///
    /// The number of tables in the set.
    pub fn len(&self) -> u32 {
        self.words.iter().map(|word| word.count_ones()).sum()
    }

    /// Adds every table of `other`.
    pub fn union_with(&mut self, other: &Self) {
        for (word, other_word) in self.words.iter_mut().zip(other.words.iter()) {
            *word |= *other_word;
        }
    }

    /// Keeps only the tables also in `other`.
    pub fn intersect_with(&mut self, other: &Self) {
        for (word, other_word) in self.words.iter_mut().zip(other.words.iter()) {
            *word &= *other_word;
        }
    }

    /// Removes every table of `other`.
    pub fn subtract(&mut self, other: &Self) {
        for (word, other_word) in self.words.iter_mut().zip(other.words.iter()) {
            *word &= !*other_word;
        }
    }

    /// # Returns
    ///
    /// The complement of the set within its range.
    pub fn complement(&self) -> Self {
        let mut complement = Self {
            words: self.words.iter().map(|word| !*word).collect(),
            num_tables: self.num_tables,
        };
        complement.mask_tail();
        complement
    }

    /// # Returns
    ///
    /// The tables of the set in ascending ordinal order.
    pub fn iter(&self) -> impl Iterator<Item = TableOrdinal> + '_ {
        (0_u32..)
            .zip(self.words.iter())
            .flat_map(|(word_index, word)| {
                let mut remaining = *word;
                std::iter::from_fn(move || {
                    if 0 == remaining {
                        return None;
                    }
                    let bit = remaining.trailing_zeros();
                    remaining &= remaining - 1;
                    Some(TableOrdinal::new(word_index * u64::BITS + bit))
                })
            })
    }

    /// Clears the bits past the last table.
    fn mask_tail(&mut self) {
        let num_tail_bits = self.num_tables % u64::BITS;
        if let Some(last) = self.words.last_mut()
            && 0 != num_tail_bits
        {
            *last &= (1_u64 << num_tail_bits) - 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use clp_s_format::TableOrdinal;

    use super::TableBits;

    /// # Returns
    ///
    /// The set of `tables` over `num_tables` tables.
    fn bits(num_tables: u32, tables: &[u32]) -> TableBits {
        let mut set = TableBits::empty(num_tables);
        for table in tables {
            set.insert(TableOrdinal::new(*table));
        }
        set
    }

    /// # Returns
    ///
    /// The ordinals of `set`, ascending.
    fn ordinals(set: &TableBits) -> Vec<u32> {
        set.iter().map(TableOrdinal::get).collect()
    }

    #[test]
    fn set_operations_follow_set_algebra() {
        let left = bits(130, &[0, 5, 64, 129]);
        let right = bits(130, &[5, 64, 100]);

        let mut union = left.clone();
        union.union_with(&right);
        assert_eq!(ordinals(&union), [0, 5, 64, 100, 129]);

        let mut intersection = left.clone();
        intersection.intersect_with(&right);
        assert_eq!(ordinals(&intersection), [5, 64]);

        let mut difference = left.clone();
        difference.subtract(&right);
        assert_eq!(ordinals(&difference), [0, 129]);

        assert_eq!(left.len(), 4);
        assert_eq!(left.complement().len(), 126);
        assert!(!left.complement().contains(TableOrdinal::new(129)));
        assert!(left.complement().contains(TableOrdinal::new(128)));
    }

    #[test]
    fn full_sets_have_clear_tail_bits() {
        let full = TableBits::full(65);
        assert_eq!(full.len(), 65);
        assert_eq!(ordinals(&full.complement()), Vec::<u32>::new());
        assert_eq!(ordinals(&TableBits::full(0)), Vec::<u32>::new());
    }

    #[test]
    fn out_of_range_ordinals_are_ignored() {
        let mut set = TableBits::empty(3);
        set.insert(TableOrdinal::new(3));
        set.insert(TableOrdinal::new(1_000));
        assert_eq!(ordinals(&set), Vec::<u32>::new());
        assert!(!set.contains(TableOrdinal::new(3)));
    }
}
