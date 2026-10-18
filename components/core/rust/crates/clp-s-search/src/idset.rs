//! Adaptive sets of dictionary ids.

use std::fmt;

/// A non-empty set of dictionary ids, represented by density: one id, a small sorted list, or a
/// bitset once the ids are dense.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdSet {
    Single(u64),

    /// Sorted, distinct ids.
    Sorted(Box<[u64]>),

    /// Bit `id - base` of the words is set for every id.
    Dense {
        base: u64,
        words: Box<[u64]>,
    },
}

impl IdSet {
    /// Factory function.
    ///
    /// Picks the representation from the number and spread of the ids: a bitset when it is no
    /// larger than the sorted list.
    ///
    /// # Returns
    ///
    /// The set of `ids`, or `None` if `ids` is empty.
    #[must_use]
    pub fn from_ids(mut ids: Vec<u64>) -> Option<Self> {
        ids.sort_unstable();
        ids.dedup();
        let (&first, &last) = (ids.first()?, ids.last()?);
        if 1 == ids.len() {
            return Some(Self::Single(first));
        }
        let num_words = (last - first) / u64::from(u64::BITS) + 1;
        if num_words >= ids.len() as u64 {
            return Some(Self::Sorted(ids.into_boxed_slice()));
        }
        let mut words = vec![0_u64; usize::try_from(num_words).ok()?];
        for id in ids {
            let offset = id - first;
            if let Some(word) = usize::try_from(offset / u64::from(u64::BITS))
                .ok()
                .and_then(|index| words.get_mut(index))
            {
                *word |= 1 << (offset % u64::from(u64::BITS));
            }
        }
        Some(Self::Dense {
            base: first,
            words: words.into_boxed_slice(),
        })
    }

    /// # Returns
    ///
    /// Whether `id` is in the set.
    #[must_use]
    pub fn contains(&self, id: u64) -> bool {
        match self {
            Self::Single(single) => *single == id,
            Self::Sorted(ids) => ids.binary_search(&id).is_ok(),
            Self::Dense { base, words } => id.checked_sub(*base).is_some_and(|offset| {
                usize::try_from(offset / u64::from(u64::BITS))
                    .ok()
                    .and_then(|index| words.get(index))
                    .is_some_and(|word| 0 != word & (1 << (offset % u64::from(u64::BITS))))
            }),
        }
    }

    /// # Returns
    ///
    /// The number of ids in the set.
    #[must_use]
    pub fn len(&self) -> u64 {
        match self {
            Self::Single(_) => 1,
            Self::Sorted(ids) => ids.len() as u64,
            Self::Dense { words, .. } => {
                words.iter().map(|word| u64::from(word.count_ones())).sum()
            }
        }
    }

    /// # Returns
    ///
    /// Whether the set is empty (never, by construction).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// # Returns
    ///
    /// The ids in ascending order.
    #[must_use]
    pub fn to_vec(&self) -> Vec<u64> {
        match self {
            Self::Single(single) => vec![*single],
            Self::Sorted(ids) => ids.to_vec(),
            Self::Dense { base, words } => (0_u64..)
                .zip(words.iter())
                .flat_map(|(word_index, word)| {
                    (0..u64::from(u64::BITS))
                        .filter(move |bit| 0 != word & (1 << bit))
                        .map(move |bit| base + word_index * u64::from(u64::BITS) + bit)
                })
                .collect(),
        }
    }

    /// # Returns
    ///
    /// The heap bytes the set holds (for plan limits).
    #[must_use]
    pub fn heap_size(&self) -> u64 {
        match self {
            Self::Single(_) => 0,
            Self::Sorted(ids) => ids.len() as u64 * 8,
            Self::Dense { words, .. } => words.len() as u64 * 8,
        }
    }
}

impl fmt::Display for IdSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        const MAX_LISTED_IDS: usize = 8;

        let ids = self.to_vec();
        formatter.write_str("{")?;
        for (index, id) in ids.iter().take(MAX_LISTED_IDS).enumerate() {
            if 0 != index {
                formatter.write_str(", ")?;
            }
            write!(formatter, "{id}")?;
        }
        if ids.len() > MAX_LISTED_IDS {
            write!(formatter, ", ... ({} ids)", ids.len())?;
        }
        formatter.write_str("}")
    }
}

#[cfg(test)]
mod tests {
    use super::IdSet;

    #[test]
    fn representation_follows_density() {
        assert_eq!(IdSet::from_ids(Vec::new()), None);
        assert_eq!(IdSet::from_ids(vec![7, 7]), Some(IdSet::Single(7)));
        assert_eq!(
            IdSet::from_ids(vec![1_000, 3, 70]),
            Some(IdSet::Sorted(vec![3, 70, 1_000].into_boxed_slice()))
        );
        let dense = IdSet::from_ids((10..200).collect()).expect("non-empty ids");
        assert!(matches!(dense, IdSet::Dense { base: 10, .. }), "{dense:?}");
    }

    #[test]
    fn every_representation_answers_membership_exactly() {
        let inputs: [Vec<u64>; 3] = [
            vec![5],
            vec![2, 900, 4_000_000],
            (100..400).step_by(3).collect(),
        ];
        for ids in inputs {
            let set = IdSet::from_ids(ids.clone()).expect("non-empty ids");
            for probe in 0..5_000 {
                assert_eq!(set.contains(probe), ids.contains(&probe), "{set:?} {probe}");
            }
            assert!(!set.contains(u64::MAX));
            assert_eq!(set.to_vec(), ids);
            assert_eq!(set.len(), ids.len() as u64);
        }
    }

    #[test]
    fn display_lists_a_bounded_number_of_ids() {
        let set = IdSet::from_ids((0..20).map(|id| id * 1_000).collect()).expect("non-empty ids");
        assert_eq!(
            set.to_string(),
            "{0, 1000, 2000, 3000, 4000, 5000, 6000, 7000, ... (20 ids)}"
        );
    }
}
