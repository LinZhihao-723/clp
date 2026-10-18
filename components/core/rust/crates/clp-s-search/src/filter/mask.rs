//! Row masks of one window (one bit per row, laid out like [`clp_s_table::Selection`]) and the
//! pool that recycles them.

/// Recycles the masks of one window size.
#[derive(Debug, Default)]
pub struct MaskPool {
    free: Vec<Vec<u64>>,
    num_words: usize,
}

impl MaskPool {
    /// Sets the number of words of the masks handed out from now on.
    pub const fn set_num_words(&mut self, num_words: usize) {
        self.num_words = num_words;
    }

    /// # Returns
    ///
    /// A mask with no row set.
    pub fn zeroed(&mut self) -> Vec<u64> {
        let mut mask = self.free.pop().unwrap_or_default();
        mask.clear();
        mask.resize(self.num_words, 0);
        mask
    }

    /// # Returns
    ///
    /// A mask with rows `0..num_rows` set.
    pub fn full(&mut self, num_rows: u32) -> Vec<u64> {
        let mut mask = self.zeroed();
        mask.fill(u64::MAX);
        let num_tail_bits = num_rows % u64::BITS;
        if 0 != num_tail_bits
            && let Some(last) = mask.last_mut()
        {
            *last = (1 << num_tail_bits) - 1;
        }
        mask
    }

    /// # Returns
    ///
    /// A copy of `source`.
    pub fn copy_of(&mut self, source: &[u64]) -> Vec<u64> {
        let mut mask = self.free.pop().unwrap_or_default();
        mask.clear();
        mask.extend_from_slice(source);
        mask
    }

    /// # Returns
    ///
    /// The mask `source & !excluded`.
    pub fn and_not(&mut self, source: &[u64], excluded: &[u64]) -> Vec<u64> {
        let mut mask = self.copy_of(source);
        and_not_assign(&mut mask, excluded);
        mask
    }

    /// Returns `mask` to the pool.
    pub fn recycle(&mut self, mask: Vec<u64>) {
        self.free.push(mask);
    }
}

/// # Returns
///
/// Whether no row of `mask` is set.
pub fn is_empty(mask: &[u64]) -> bool {
    mask.iter().all(|word| 0 == *word)
}

/// Sets in `target` every row set in `source`.
pub fn or_assign(target: &mut [u64], source: &[u64]) {
    for (word, source_word) in target.iter_mut().zip(source) {
        *word |= source_word;
    }
}

/// Clears in `target` every row set in `excluded`.
pub fn and_not_assign(target: &mut [u64], excluded: &[u64]) {
    for (word, excluded_word) in target.iter_mut().zip(excluded) {
        *word &= !excluded_word;
    }
}

/// Calls `visit` with every row set in `mask`, in ascending order, until it fails.
///
/// # Type Parameters
///
/// * `ErrorType` - The error `visit` can fail with.
/// * `VisitorType` - Receives each set row.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards `visit`'s return values on failure.
pub fn try_for_each_row<ErrorType, VisitorType: FnMut(u32) -> Result<(), ErrorType>>(
    mask: &[u64],
    mut visit: VisitorType,
) -> Result<(), ErrorType> {
    for (word_base, word) in (0_u32..).step_by(64).zip(mask.iter().copied()) {
        let mut remaining = word;
        while 0 != remaining {
            visit(word_base + remaining.trailing_zeros())?;
            remaining &= remaining - 1;
        }
    }
    Ok(())
}

/// Writes, for every word of `active` with a set row, the rows of that word on which `predicate`
/// holds into `truthy` and (if `falsy` is given) the others into `falsy`; both stay within
/// `active`. Words without an active row are left untouched.
///
/// The predicate runs on every row of an active word, active or not, so it must be cheap and
/// total over the window (it receives window rows below the number of values).
///
/// # Type Parameters
///
/// * `PredicateType` - Decides one window row.
pub fn write_word_masks<PredicateType: Fn(usize) -> bool>(
    active: &[u64],
    num_rows: usize,
    truthy: &mut [u64],
    mut falsy: Option<&mut [u64]>,
    predicate: PredicateType,
) {
    for (word_index, active_word) in active.iter().copied().enumerate() {
        if 0 == active_word {
            continue;
        }
        let base = word_index * 64;
        let end = num_rows.min(base + 64);
        let mut bits = 0_u64;
        for row in base..end {
            bits |= u64::from(predicate(row)) << (row - base);
        }
        truthy[word_index] = bits & active_word;
        if let Some(falsy) = falsy.as_deref_mut() {
            falsy[word_index] = !bits & active_word;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// # Returns
    ///
    /// The rows set in `mask`.
    fn rows(mask: &[u64]) -> Vec<u32> {
        let mut rows = Vec::new();
        try_for_each_row::<(), _>(mask, |row| {
            rows.push(row);
            Ok(())
        })
        .expect("collecting rows never fails");
        rows
    }

    #[test]
    fn full_masks_cover_exactly_the_rows() {
        let mut pool = MaskPool::default();
        for (num_rows, num_words) in [
            (0_u32, 0_usize),
            (1, 1),
            (63, 1),
            (64, 1),
            (65, 2),
            (130, 3),
        ] {
            pool.set_num_words(num_words);
            let mask = pool.full(num_rows);
            assert_eq!(rows(&mask), (0..num_rows).collect::<Vec<_>>(), "{num_rows}");
            pool.recycle(mask);
            assert_eq!(pool.zeroed(), vec![0; num_words], "{num_rows}");
        }
    }

    #[test]
    fn recycled_masks_are_cleared_and_resized() {
        let mut pool = MaskPool::default();
        pool.set_num_words(3);
        let mask = pool.full(192);
        pool.recycle(mask);
        pool.set_num_words(1);
        assert_eq!(pool.zeroed(), vec![0]);
        assert_eq!(pool.copy_of(&[5, 6]), vec![5, 6]);
        assert_eq!(pool.and_not(&[0b1111], &[0b0101]), vec![0b1010]);
    }

    #[test]
    fn word_operations_combine_rows() {
        let mut target = vec![0b0011_u64, 1 << 63];
        or_assign(&mut target, &[0b0100, 1]);
        assert_eq!(target, vec![0b0111, (1 << 63) | 1]);
        and_not_assign(&mut target, &[0b0001, 1 << 63]);
        assert_eq!(target, vec![0b0110, 1]);
        assert_eq!(rows(&target), vec![1, 2, 64]);
        assert!(!is_empty(&target));
        assert!(is_empty(&[0, 0]));
    }

    #[test]
    fn word_masks_split_active_rows_by_the_predicate() {
        let num_rows = 70;
        let mut active = vec![0_u64; 2];
        for row in [0, 3, 4, 63, 64, 69] {
            active[row / 64] |= 1 << (row % 64);
        }
        let mut truthy = vec![u64::MAX, u64::MAX];
        let mut falsy = vec![0_u64; 2];
        write_word_masks(&active, num_rows, &mut truthy, Some(&mut falsy), |row| {
            0 == row % 3
        });
        assert_eq!(rows(&truthy), vec![0, 3, 63, 69]);
        assert_eq!(rows(&falsy), vec![4, 64]);

        let mut untouched = vec![7_u64, 0];
        write_word_masks(&[0, 1], num_rows, &mut untouched, None, |_| true);
        assert_eq!(untouched, vec![7, 1]);
    }
}
