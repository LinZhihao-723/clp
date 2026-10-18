//! Column sets and the two-phase column demand of a scan.

use clp_s_format::DictionaryNeeds;

use crate::ColumnIndex;
use crate::TableLayout;

/// Dense bitset over the columns of one table layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSet {
    words: Box<[u64]>,
    num_columns: u32,
}

impl ColumnSet {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// An empty set over a layout of `num_columns` columns.
    #[must_use]
    pub fn empty(num_columns: u32) -> Self {
        Self {
            words: vec![0; num_columns.div_ceil(u64::BITS) as usize].into_boxed_slice(),
            num_columns,
        }
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The set of every column of a layout of `num_columns` columns.
    #[must_use]
    pub fn full(num_columns: u32) -> Self {
        let mut set = Self::empty(num_columns);
        set.words.fill(u64::MAX);
        let num_tail_bits = num_columns % u64::BITS;
        if let Some(last) = set.words.last_mut()
            && 0 != num_tail_bits
        {
            *last &= (1_u64 << num_tail_bits) - 1;
        }
        set
    }

    /// # Returns
    ///
    /// The number of columns of the layout the set was created for.
    #[must_use]
    pub const fn num_columns(&self) -> u32 {
        self.num_columns
    }

    /// Adds `column` to the set.
    ///
    /// # Panics
    ///
    /// Panics if `column` is outside the layout the set was created for.
    pub fn insert(&mut self, column: ColumnIndex) {
        assert!(
            column.get() < self.num_columns,
            "column {} is outside a layout of {} columns",
            column.get(),
            self.num_columns
        );
        self.words[word_index(column)] |= bit(column);
    }

    /// Adds every column of `other` to the set.
    ///
    /// # Panics
    ///
    /// Panics if the two sets were created for layouts of different sizes.
    pub fn union_with(&mut self, other: &Self) {
        assert_eq!(
            self.num_columns, other.num_columns,
            "column sets of different layouts"
        );
        for (word, other_word) in self.words.iter_mut().zip(other.words.iter()) {
            *word |= other_word;
        }
    }

    /// Removes every column of `other` from the set.
    ///
    /// # Panics
    ///
    /// Panics if the two sets were created for layouts of different sizes.
    pub fn subtract(&mut self, other: &Self) {
        assert_eq!(
            self.num_columns, other.num_columns,
            "column sets of different layouts"
        );
        for (word, other_word) in self.words.iter_mut().zip(other.words.iter()) {
            *word &= !other_word;
        }
    }

    /// # Returns
    ///
    /// Whether `column` is in the set.
    #[must_use]
    pub fn contains(&self, column: ColumnIndex) -> bool {
        column.get() < self.num_columns && 0 != self.words[word_index(column)] & bit(column)
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
    /// The columns of the set in ascending order.
    #[must_use]
    pub fn to_vec(&self) -> Vec<ColumnIndex> {
        self.iter().collect()
    }

    /// # Returns
    ///
    /// An iterator over the columns of the set in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = ColumnIndex> + '_ {
        (0_u32..)
            .step_by(64)
            .zip(self.words.iter().copied())
            .flat_map(|(base, remaining)| SetBits { base, remaining })
    }
}

/// Which columns a scan decodes: evaluation columns for every window, output columns only for
/// windows with at least one selected row. Columns outside both sets are never sliced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnDemand {
    evaluation: ColumnSet,
    output: ColumnSet,
}

impl ColumnDemand {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The demand; `output` columns that are also in `evaluation` are decoded once.
    ///
    /// # Panics
    ///
    /// Panics if the two sets were created for layouts of different sizes.
    #[must_use]
    pub fn new(evaluation: ColumnSet, output: ColumnSet) -> Self {
        assert_eq!(
            evaluation.num_columns, output.num_columns,
            "evaluation and output column sets of different layouts"
        );
        Self { evaluation, output }
    }

    /// # Returns
    ///
    /// The evaluation columns.
    #[must_use]
    pub const fn evaluation(&self) -> &ColumnSet {
        &self.evaluation
    }

    /// # Returns
    ///
    /// The output columns.
    #[must_use]
    pub const fn output(&self) -> &ColumnSet {
        &self.output
    }

    /// # Returns
    ///
    /// The dictionaries the demanded columns of `layout` read.
    #[must_use]
    pub fn dictionary_needs(&self, layout: &TableLayout) -> DictionaryNeeds {
        let columns = layout.columns();
        self.evaluation
            .iter()
            .chain(self.output.iter())
            .filter_map(|column| columns.get(column.get() as usize))
            .fold(DictionaryNeeds::default(), |needs, spec| {
                needs.union(spec.node_type.referenced_dictionaries())
            })
    }
}

/// The columns of one word of a [`ColumnSet`], in ascending order.
struct SetBits {
    /// The column of bit 0.
    base: u32,

    remaining: u64,
}

impl Iterator for SetBits {
    type Item = ColumnIndex;

    fn next(&mut self) -> Option<Self::Item> {
        if 0 == self.remaining {
            return None;
        }
        let column = ColumnIndex::new(self.base + self.remaining.trailing_zeros());
        self.remaining &= self.remaining - 1;
        Some(column)
    }
}

/// # Returns
///
/// The index of the word that holds `column`.
const fn word_index(column: ColumnIndex) -> usize {
    (column.get() / u64::BITS) as usize
}

/// # Returns
///
/// The bit of `column` inside its word.
const fn bit(column: ColumnIndex) -> u64 {
    1 << (column.get() % u64::BITS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// # Returns
    ///
    /// The set over `num_columns` columns holding `columns`.
    fn set_of(num_columns: u32, columns: &[u32]) -> ColumnSet {
        let mut set = ColumnSet::empty(num_columns);
        for column in columns {
            set.insert(ColumnIndex::new(*column));
        }
        set
    }

    /// # Returns
    ///
    /// The columns of `set` as integers.
    fn members(set: &ColumnSet) -> Vec<u32> {
        set.to_vec().into_iter().map(ColumnIndex::get).collect()
    }

    #[test]
    fn empty_and_full_sets() {
        for num_columns in [0, 1, 63, 64, 65, 200] {
            let empty = ColumnSet::empty(num_columns);
            assert!(empty.is_empty(), "{num_columns} columns");
            assert_eq!(members(&empty), Vec::<u32>::new());

            let full = ColumnSet::full(num_columns);
            assert_eq!(
                members(&full),
                (0..num_columns).collect::<Vec<_>>(),
                "{num_columns} columns"
            );
            assert!(!full.contains(ColumnIndex::new(num_columns)));
        }
    }

    #[test]
    fn insert_contains_and_iterate_in_order() {
        let set = set_of(130, &[129, 0, 64, 63, 64]);
        assert_eq!(members(&set), vec![0, 63, 64, 129]);
        assert!(set.contains(ColumnIndex::new(63)));
        assert!(!set.contains(ColumnIndex::new(62)));
        assert!(!set.contains(ColumnIndex::new(1000)));
        assert!(!set.is_empty());
    }

    #[test]
    #[should_panic(expected = "column 3 is outside a layout of 3 columns")]
    fn inserting_a_column_outside_the_layout_panics() {
        ColumnSet::empty(3).insert(ColumnIndex::new(3));
    }

    #[test]
    fn union_and_subtract() {
        let mut set = set_of(70, &[1, 65]);
        set.union_with(&set_of(70, &[2, 65, 69]));
        assert_eq!(members(&set), vec![1, 2, 65, 69]);
        set.subtract(&set_of(70, &[1, 69, 3]));
        assert_eq!(members(&set), vec![2, 65]);
    }

    #[test]
    #[should_panic(expected = "column sets of different layouts")]
    fn union_of_different_layouts_panics() {
        ColumnSet::empty(3).union_with(&ColumnSet::empty(4));
    }

    #[test]
    fn demand_keeps_both_phases() {
        let demand = ColumnDemand::new(set_of(5, &[0]), set_of(5, &[0, 3]));
        assert_eq!(members(demand.evaluation()), vec![0]);
        assert_eq!(members(demand.output()), vec![0, 3]);
    }
}
