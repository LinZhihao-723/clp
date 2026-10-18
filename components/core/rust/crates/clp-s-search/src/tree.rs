//! Iterative folds over normalized query trees.

use std::collections::HashSet;

use clp_s_query::LeafId;
use clp_s_query::NormalExpr;

/// One node of a [`NormalExpr`] with its children already folded.
pub enum FoldNode<ValueType> {
    Constant(bool),
    Leaf { leaf: LeafId, is_negated: bool },
    And(Vec<ValueType>),
    Or(Vec<ValueType>),
}

/// The state of one leaf occurrence within one scope (a table).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OccurrenceState {
    /// The occurrence is True on every row.
    Always,

    /// The occurrence is True on no row.
    Never,

    /// The occurrence must be evaluated per row.
    Varies,
}

/// The result of folding a tree under occurrence states.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Folded {
    Always,
    Never,
    Residual(NormalExpr),
}

/// Folds `root` bottom-up without recursion: `combine` receives every node after its children.
///
/// # Type Parameters
///
/// * `ValueType` - The folded value of a node.
/// * `CombineType` - Folds one node from the values of its children.
///
/// # Returns
///
/// The folded value of the root.
pub fn fold<ValueType, CombineType: FnMut(FoldNode<ValueType>) -> ValueType>(
    root: &NormalExpr,
    mut combine: CombineType,
) -> ValueType {
    enum Frame<'tree> {
        Visit(&'tree NormalExpr),
        BuildAnd(usize),
        BuildOr(usize),
    }

    let mut frames = vec![Frame::Visit(root)];
    let mut values: Vec<ValueType> = Vec::new();
    while let Some(frame) = frames.pop() {
        let value = match frame {
            Frame::Visit(node) => match node {
                NormalExpr::MatchAll => combine(FoldNode::Constant(true)),
                NormalExpr::MatchNone => combine(FoldNode::Constant(false)),
                NormalExpr::Leaf { leaf, is_negated } => combine(FoldNode::Leaf {
                    leaf: *leaf,
                    is_negated: *is_negated,
                }),
                NormalExpr::And(children) | NormalExpr::Or(children) => {
                    frames.push(if matches!(node, NormalExpr::And(_)) {
                        Frame::BuildAnd(children.len())
                    } else {
                        Frame::BuildOr(children.len())
                    });
                    frames.extend(children.iter().rev().map(Frame::Visit));
                    continue;
                }
            },
            Frame::BuildAnd(num_children) => {
                let children = values.split_off(values.len() - num_children);
                combine(FoldNode::And(children))
            }
            Frame::BuildOr(num_children) => {
                let children = values.split_off(values.len() - num_children);
                combine(FoldNode::Or(children))
            }
        };
        values.push(value);
    }
    // The root's frame is the first pushed and the last built, so exactly its value remains.
    values
        .pop()
        .expect("the root's value remains after every frame is built")
}

/// # Returns
///
/// The distinct leaf occurrences `(leaf, is_negated)` of `root`, in first-visit order.
pub fn occurrences(root: &NormalExpr) -> Vec<(LeafId, bool)> {
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node {
            NormalExpr::Leaf { leaf, is_negated } => {
                if seen.insert((*leaf, *is_negated)) {
                    found.push((*leaf, *is_negated));
                }
            }
            NormalExpr::And(children) | NormalExpr::Or(children) => {
                stack.extend(children.iter().rev());
            }
            NormalExpr::MatchAll | NormalExpr::MatchNone => {}
        }
    }
    found
}

/// Folds `root` under the occurrence states `state_of` returns: constant occurrences disappear
/// and the remaining connectives are flattened.
///
/// # Type Parameters
///
/// * `StateType` - Returns the state of an occurrence.
///
/// # Returns
///
/// The residual of `root`, or the constant it folds to.
pub fn fold_residual<StateType: FnMut(LeafId, bool) -> OccurrenceState>(
    root: &NormalExpr,
    mut state_of: StateType,
) -> Folded {
    fold(root, |node| match node {
        FoldNode::Constant(true) => Folded::Always,
        FoldNode::Constant(false) => Folded::Never,
        FoldNode::Leaf { leaf, is_negated } => match state_of(leaf, is_negated) {
            OccurrenceState::Always => Folded::Always,
            OccurrenceState::Never => Folded::Never,
            OccurrenceState::Varies => Folded::Residual(NormalExpr::Leaf { leaf, is_negated }),
        },
        FoldNode::And(children) => connective(children, true),
        FoldNode::Or(children) => connective(children, false),
    })
}

/// # Returns
///
/// The folded AND (or OR) of folded children.
fn connective(children: Vec<Folded>, is_and: bool) -> Folded {
    let (absorbing, identity) = if is_and {
        (Folded::Never, Folded::Always)
    } else {
        (Folded::Always, Folded::Never)
    };
    let mut residuals = Vec::with_capacity(children.len());
    for child in children {
        match child {
            Folded::Residual(NormalExpr::And(grandchildren)) if is_and => {
                residuals.extend(grandchildren.into_vec());
            }
            Folded::Residual(NormalExpr::Or(grandchildren)) if !is_and => {
                residuals.extend(grandchildren.into_vec());
            }
            Folded::Residual(residual) => residuals.push(residual),
            constant if constant == absorbing => return absorbing,
            _ => {}
        }
    }
    let mut distinct: Vec<NormalExpr> = Vec::with_capacity(residuals.len());
    for residual in residuals {
        if !distinct.contains(&residual) {
            distinct.push(residual);
        }
    }
    match distinct.len() {
        0 => identity,
        1 => distinct.pop().map_or(identity, Folded::Residual),
        _ if is_and => Folded::Residual(NormalExpr::And(distinct.into_boxed_slice())),
        _ => Folded::Residual(NormalExpr::Or(distinct.into_boxed_slice())),
    }
}

#[cfg(test)]
mod tests {
    use clp_s_query::LeafId;
    use clp_s_query::NormalExpr;

    use super::Folded;
    use super::OccurrenceState;
    use super::fold_residual;
    use super::occurrences;

    /// # Returns
    ///
    /// The occurrence of leaf `id`.
    fn leaf(id: u32, is_negated: bool) -> NormalExpr {
        NormalExpr::Leaf {
            leaf: LeafId::new(id),
            is_negated,
        }
    }

    #[test]
    fn folding_drops_constants_and_flattens() {
        let root = NormalExpr::And(
            vec![
                leaf(0, false),
                NormalExpr::Or(vec![leaf(1, false), leaf(2, true)].into_boxed_slice()),
                NormalExpr::Or(vec![leaf(3, false), leaf(4, false)].into_boxed_slice()),
            ]
            .into_boxed_slice(),
        );
        let folded = fold_residual(&root, |leaf, _| match leaf.get() {
            0 | 3 => OccurrenceState::Varies,
            2 => OccurrenceState::Always,
            _ => OccurrenceState::Never,
        });
        assert_eq!(
            folded,
            Folded::Residual(NormalExpr::And(
                vec![leaf(0, false), leaf(3, false)].into_boxed_slice()
            ))
        );

        let never = fold_residual(&root, |leaf, _| {
            if 0 == leaf.get() {
                OccurrenceState::Never
            } else {
                OccurrenceState::Varies
            }
        });
        assert_eq!(never, Folded::Never);
        let always = fold_residual(&root, |_, _| OccurrenceState::Always);
        assert_eq!(always, Folded::Always);
    }

    #[test]
    fn nested_connectives_of_the_same_kind_flatten() {
        let root = NormalExpr::Or(
            vec![
                NormalExpr::And(vec![leaf(0, false), leaf(1, false)].into_boxed_slice()),
                NormalExpr::Or(vec![leaf(2, false), leaf(3, false)].into_boxed_slice()),
            ]
            .into_boxed_slice(),
        );
        let folded = fold_residual(&root, |leaf, _| {
            if 1 == leaf.get() {
                OccurrenceState::Always
            } else {
                OccurrenceState::Varies
            }
        });
        assert_eq!(
            folded,
            Folded::Residual(NormalExpr::Or(
                vec![leaf(0, false), leaf(2, false), leaf(3, false)].into_boxed_slice()
            ))
        );
    }

    #[test]
    fn trees_at_the_depth_limit_fold() {
        let mut root = leaf(0, false);
        for depth in 0..1_024 {
            let other = leaf(1 + depth % 3, false);
            root = if 0 == depth % 2 {
                NormalExpr::And(vec![root, other].into_boxed_slice())
            } else {
                NormalExpr::Or(vec![root, other].into_boxed_slice())
            };
        }
        let found: Vec<u32> = occurrences(&root)
            .iter()
            .map(|(leaf, _)| leaf.get())
            .collect();
        assert_eq!(found, [0, 1, 2, 3]);
        assert_eq!(
            fold_residual(&root, |_, _| OccurrenceState::Never),
            Folded::Never
        );
        assert_eq!(
            fold_residual(&root, |_, _| OccurrenceState::Always),
            Folded::Always
        );
    }
}
