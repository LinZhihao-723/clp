//! Row selections of one window.

/// The selected rows of one window, one bit per row (bit `i % 64` of word `i / 64`); bits past
/// `num_rows` are always clear. The shared vocabulary of kernels, scans and delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    words: Vec<u64>,
    num_rows: u32,
}

impl Selection {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A selection of `num_rows` rows with no row selected.
    #[must_use]
    pub fn empty(num_rows: u32) -> Self {
        Self {
            words: vec![0; num_words(num_rows)],
            num_rows,
        }
    }

    /// # Returns
    ///
    /// The number of rows the selection covers.
    #[must_use]
    pub const fn num_rows(&self) -> u32 {
        self.num_rows
    }

    /// # Returns
    ///
    /// The number of selected rows.
    #[must_use]
    pub fn count(&self) -> u32 {
        self.words.iter().map(|word| word.count_ones()).sum()
    }

    /// # Returns
    ///
    /// Whether `row` is selected (false for rows past [`Self::num_rows`]).
    #[must_use]
    pub fn is_selected(&self, row: u32) -> bool {
        row < self.num_rows && 0 != self.words[word_index(row)] & bit(row)
    }

    /// Selects `row`.
    ///
    /// # Panics
    ///
    /// Panics if `row` is not below [`Self::num_rows`].
    pub fn select(&mut self, row: u32) {
        assert!(
            row < self.num_rows,
            "row {row} is outside a selection of {} rows",
            self.num_rows
        );
        self.words[word_index(row)] |= bit(row);
    }

    /// Selects every row.
    pub fn select_all(&mut self) {
        self.words.fill(u64::MAX);
        self.mask_tail();
    }

    /// Deselects every row and resizes the selection to `num_rows` rows.
    pub fn reset(&mut self, num_rows: u32) {
        self.words.clear();
        self.words.resize(num_words(num_rows), 0);
        self.num_rows = num_rows;
    }

    /// # Returns
    ///
    /// The selection words.
    #[must_use]
    pub fn words(&self) -> &[u64] {
        &self.words
    }

    /// Returns the selection words for kernels that write whole words; callers must keep the tail
    /// bits past [`Self::num_rows`] clear (or call [`Self::mask_tail`]).
    ///
    /// # Returns
    ///
    /// The selection words.
    pub fn words_mut(&mut self) -> &mut [u64] {
        &mut self.words
    }

    /// Clears the tail bits past [`Self::num_rows`].
    pub fn mask_tail(&mut self) {
        let num_tail_bits = self.num_rows % u64::BITS;
        if 0 == num_tail_bits {
            return;
        }
        if let Some(last) = self.words.last_mut() {
            *last &= (1_u64 << num_tail_bits) - 1;
        }
    }

    /// Appends the selected window-relative rows, in ascending order, to `output`.
    pub fn extend_indices(&self, output: &mut Vec<u32>) {
        for (word_base, word) in (0_u32..).step_by(64).zip(self.words.iter().copied()) {
            let mut remaining = word;
            while 0 != remaining {
                output.push(word_base + remaining.trailing_zeros());
                remaining &= remaining - 1;
            }
        }
    }
}

/// # Returns
///
/// The number of words that hold `num_rows` bits.
const fn num_words(num_rows: u32) -> usize {
    num_rows.div_ceil(u64::BITS) as usize
}

/// # Returns
///
/// The index of the word that holds `row`.
const fn word_index(row: u32) -> usize {
    (row / u64::BITS) as usize
}

/// # Returns
///
/// The bit of `row` inside its word.
const fn bit(row: u32) -> u64 {
    1 << (row % u64::BITS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// # Returns
    ///
    /// The selected rows of `selection`.
    fn indices(selection: &Selection) -> Vec<u32> {
        let mut rows = Vec::new();
        selection.extend_indices(&mut rows);
        rows
    }

    #[test]
    fn empty_selection_selects_nothing() {
        let selection = Selection::empty(130);
        assert_eq!(selection.num_rows(), 130);
        assert_eq!(selection.count(), 0);
        assert_eq!(selection.words(), &[0, 0, 0]);
        assert_eq!(indices(&selection), Vec::<u32>::new());
    }

    #[test]
    fn select_sets_one_bit_per_row() {
        let mut selection = Selection::empty(130);
        for row in [0, 63, 64, 129] {
            selection.select(row);
        }
        assert_eq!(selection.words(), &[1 | 1 << 63, 1, 1 << 1]);
        assert_eq!(selection.count(), 4);
        assert_eq!(indices(&selection), vec![0, 63, 64, 129]);
        assert!(selection.is_selected(129));
        assert!(!selection.is_selected(128));
        assert!(!selection.is_selected(130));
        assert!(!selection.is_selected(u32::MAX));
    }

    #[test]
    #[should_panic(expected = "row 130 is outside a selection of 130 rows")]
    fn selecting_a_row_past_the_end_panics() {
        Selection::empty(130).select(130);
    }

    #[test]
    fn select_all_keeps_the_tail_clear() {
        for num_rows in [0, 1, 63, 64, 65, 4096, 4097] {
            let mut selection = Selection::empty(num_rows);
            selection.select_all();
            assert_eq!(selection.count(), num_rows, "{num_rows} rows");
            assert_eq!(
                indices(&selection),
                (0..num_rows).collect::<Vec<_>>(),
                "{num_rows} rows"
            );
        }
    }

    #[test]
    fn mask_tail_clears_bits_written_past_the_end() {
        let mut selection = Selection::empty(70);
        selection.words_mut().fill(u64::MAX);
        selection.mask_tail();
        assert_eq!(selection.words(), &[u64::MAX, (1 << 6) - 1]);
        assert_eq!(selection.count(), 70);

        let mut aligned = Selection::empty(128);
        aligned.words_mut().fill(u64::MAX);
        aligned.mask_tail();
        assert_eq!(aligned.words(), &[u64::MAX, u64::MAX]);
    }

    #[test]
    fn reset_clears_and_resizes() {
        let mut selection = Selection::empty(200);
        selection.select_all();
        selection.reset(10);
        assert_eq!(selection, Selection::empty(10));
        selection.select(9);
        selection.reset(300);
        assert_eq!(selection, Selection::empty(300));
    }

    #[test]
    fn extend_indices_appends_after_existing_rows() {
        let mut selection = Selection::empty(100);
        selection.select(5);
        selection.select(99);
        let mut rows = vec![42];
        selection.extend_indices(&mut rows);
        assert_eq!(rows, vec![42, 5, 99]);
    }
}
