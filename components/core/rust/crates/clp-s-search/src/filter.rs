//! Compiled table filters: NNF trees of vectorized, polarity-aware kernels.

mod array;
mod kernel;
mod mask;
mod value;

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use clp_s_query::LeafId;
use clp_s_table::ColumnSet;
use clp_s_table::TableLayout;
use clp_s_table::WindowColumns;
use clp_s_table::WindowParts;

use crate::ArchiveSearchPlan;
use crate::EvalError;
use crate::ResidualFilter;
use crate::filter::kernel::CostClass;
use crate::filter::kernel::KernelBuffers;
use crate::filter::kernel::LeafKernel;
use crate::filter::mask::MaskPool;
use crate::tree;
use crate::tree::FoldNode;

/// The executable filter of one table: an NNF tree of batch kernels bound to its columns.
#[derive(Debug)]
pub struct CompiledTableFilter {
    /// Children before their parents; the root is the last node.
    nodes: Box<[Node]>,

    /// The children of every connective, cheapest first; connectives index ranges of it.
    children: Box<[usize]>,

    kernels: Box<[LeafKernel]>,
    required_columns: ColumnSet,
}

impl CompiledTableFilter {
    /// Factory function.
    ///
    /// Binds the residual filter of a table of `plan` to the table's loaded layout.
    ///
    /// # Returns
    ///
    /// The compiled filter on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`EvalError::PlanMismatch`] if `residual` references columns `layout` does not have.
    /// * [`EvalError::PlanMismatch`] if `residual` is not a residual of `plan` (its template is not
    ///   one of the plan's, or a leaf of the template has no binding).
    /// * Forwards [`LeafKernel::new`]'s return values on failure.
    pub fn compile(
        plan: &ArchiveSearchPlan,
        residual: &ResidualFilter,
        layout: &TableLayout,
    ) -> Result<Self, EvalError> {
        let is_plan_template = plan
            .templates()
            .get(residual.template().index())
            .is_some_and(|template| Arc::ptr_eq(template, residual.template()));
        if !is_plan_template {
            return Err(EvalError::PlanMismatch {
                reason: "the residual's template is not a template of the plan",
            });
        }
        let leaves = plan.query().leaves();
        let mut builder = Builder::default();
        let root = tree::fold(residual.root(), |node| match node {
            FoldNode::Constant(value) => {
                Ok(builder.push(Node::Constant(value), CostClass::Constant))
            }
            FoldNode::Leaf { leaf, is_negated } => {
                let kernel = builder.kernel(leaf, |leaf_id| {
                    let leaf = leaves.get(leaf_id.get() as usize);
                    let binding = residual.binding(leaf_id);
                    let (Some(leaf), Some(binding)) = (leaf, binding) else {
                        return Err(EvalError::PlanMismatch {
                            reason: "a leaf of the template has no binding",
                        });
                    };
                    LeafKernel::new(leaf, binding, layout)
                })?;
                let cost = builder.kernels[kernel].cost();
                Ok(builder.push(Node::Leaf { kernel, is_negated }, cost))
            }
            FoldNode::And(children) => builder.connective(children, true),
            FoldNode::Or(children) => builder.connective(children, false),
        })?;
        debug_assert_eq!(
            root.index + 1,
            builder.nodes.len(),
            "the root is built last"
        );

        let mut required_columns = ColumnSet::empty(layout.num_columns());
        for kernel in &builder.kernels {
            for column in kernel.columns() {
                required_columns.insert(column);
            }
        }
        Ok(Self {
            nodes: builder.nodes.into_iter().map(|(node, _)| node).collect(),
            children: builder.children.into_boxed_slice(),
            kernels: builder.kernels.into_boxed_slice(),
            required_columns,
        })
    }

    /// # Returns
    ///
    /// The columns the filter reads; the scan decodes these before any output column.
    #[must_use]
    pub const fn required_columns(&self) -> &ColumnSet {
        &self.required_columns
    }

    /// Selects, in `window.selected`, exactly the rows of the window for which the filter is True.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::run`]'s return values on failure.
    pub fn evaluate(
        &self,
        window: WindowParts<'_>,
        scratch: &mut EvaluationScratch,
    ) -> Result<(), EvalError> {
        let WindowParts { columns, selected } = window;
        let num_rows = columns.num_rows();
        scratch
            .masks
            .set_num_words(num_rows.div_ceil(u64::BITS) as usize);
        let all_rows = scratch.masks.full(num_rows);
        let result = self.run(&columns, all_rows, scratch)?;
        let words = selected.words_mut();
        let num_words = words.len().min(result.len());
        words[..num_words].copy_from_slice(&result[..num_words]);
        selected.mask_tail();
        scratch.masks.recycle(result);
        Ok(())
    }

    /// Evaluates the tree on `active` without recursion: each connective is a frame that hands its
    /// children their active rows (an AND its running selection, an OR the rows no earlier child
    /// selected) and stops once no row is left to decide.
    ///
    /// # Returns
    ///
    /// The rows of `active` on which the root is True, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`EvalError::PlanMismatch`] if the filter tree has no root.
    /// * Forwards [`Self::enter`]'s return values on failure.
    fn run(
        &self,
        columns: &WindowColumns<'_>,
        active: Vec<u64>,
        scratch: &mut EvaluationScratch,
    ) -> Result<Vec<u64>, EvalError> {
        let mut frames = std::mem::take(&mut scratch.frames);
        frames.clear();
        let root = self.nodes.len().saturating_sub(1);
        let mut finished = self.enter(root, active, &mut frames, columns, scratch)?;
        while let Some(frame) = frames.last_mut() {
            let Node::Connective {
                children: ref range,
                is_and,
            } = self.nodes[frame.node]
            else {
                break;
            };
            if let Some(result) = finished.take() {
                if is_and {
                    let previous = std::mem::replace(&mut frame.active, result);
                    scratch.masks.recycle(previous);
                } else {
                    mask::or_assign(&mut frame.selected, &result);
                    scratch.masks.recycle(result);
                }
            }
            let child_active = if frame.next_child >= range.len() {
                None
            } else if is_and {
                (!mask::is_empty(&frame.active)).then(|| scratch.masks.copy_of(&frame.active))
            } else {
                let remaining = scratch.masks.and_not(&frame.active, &frame.selected);
                if mask::is_empty(&remaining) {
                    scratch.masks.recycle(remaining);
                    None
                } else {
                    Some(remaining)
                }
            };
            let Some(child_active) = child_active else {
                let Some(frame) = frames.pop() else {
                    break;
                };
                let (result, unused) = if is_and {
                    (frame.active, frame.selected)
                } else {
                    (frame.selected, frame.active)
                };
                scratch.masks.recycle(unused);
                finished = Some(result);
                continue;
            };
            let child = self.children[range.start + frame.next_child];
            frame.next_child += 1;
            finished = self.enter(child, child_active, &mut frames, columns, scratch)?;
        }
        scratch.frames = frames;
        finished.ok_or(EvalError::PlanMismatch {
            reason: "the filter tree has no root",
        })
    }

    /// Starts evaluating `node` on `active`: a leaf or constant is evaluated at once, a connective
    /// pushes a frame.
    ///
    /// # Returns
    ///
    /// The selected rows if `node` was evaluated at once, else `None`, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`EvalError::PlanMismatch`] if `node` is not a node of the filter.
    /// * Forwards [`LeafKernel::evaluate`]'s return values on failure.
    fn enter(
        &self,
        node: usize,
        mut active: Vec<u64>,
        frames: &mut Vec<Frame>,
        columns: &WindowColumns<'_>,
        scratch: &mut EvaluationScratch,
    ) -> Result<Option<Vec<u64>>, EvalError> {
        match self.nodes.get(node) {
            None => Err(EvalError::PlanMismatch {
                reason: "a filter node index is out of range",
            }),
            Some(Node::Constant(false)) => {
                active.fill(0);
                Ok(Some(active))
            }
            Some(Node::Constant(true)) => Ok(Some(active)),
            Some(Node::Leaf { kernel, is_negated }) => {
                let selected = self.kernels[*kernel].evaluate(
                    columns,
                    &active,
                    *is_negated,
                    &mut scratch.masks,
                    &mut scratch.buffers,
                )?;
                scratch.masks.recycle(active);
                Ok(Some(selected))
            }
            Some(Node::Connective { .. }) => {
                frames.push(Frame {
                    node,
                    next_child: 0,
                    active,
                    selected: scratch.masks.zeroed(),
                });
                Ok(None)
            }
        }
    }
}

/// Reusable per-worker evaluation buffers (masks, decoded texts).
#[derive(Debug, Default)]
pub struct EvaluationScratch {
    masks: MaskPool,
    frames: Vec<Frame>,
    buffers: KernelBuffers,
}

/// One node of a compiled filter tree.
#[derive(Debug)]
enum Node {
    Constant(bool),
    Leaf {
        kernel: usize,
        is_negated: bool,
    },
    Connective {
        children: Range<usize>,
        is_and: bool,
    },
}

/// A connective being evaluated.
#[derive(Debug)]
struct Frame {
    node: usize,
    next_child: usize,

    /// For an AND, the running selection; for an OR, the rows it was given.
    active: Vec<u64>,

    /// For an OR, the rows selected so far.
    selected: Vec<u64>,
}

/// A node of the tree under construction, with its cost class.
#[derive(Clone, Copy)]
struct Built {
    index: usize,
    cost: CostClass,
}

/// Builds the nodes and kernels of a compiled filter bottom-up.
#[derive(Default)]
struct Builder {
    nodes: Vec<(Node, CostClass)>,
    children: Vec<usize>,
    kernels: Vec<LeafKernel>,
    kernel_of_leaf: HashMap<LeafId, usize>,
}

impl Builder {
    /// Appends `node`.
    ///
    /// # Returns
    ///
    /// The appended node.
    fn push(&mut self, node: Node, cost: CostClass) -> Built {
        self.nodes.push((node, cost));
        Built {
            index: self.nodes.len() - 1,
            cost,
        }
    }

    /// Returns the kernel of `leaf`, building it with `build` on its first occurrence (both
    /// polarities of a leaf share one kernel).
    ///
    /// # Type Parameters
    ///
    /// * `BuildType` - Builds the kernel of a leaf.
    ///
    /// # Returns
    ///
    /// The index of the kernel on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards `build`'s return values on failure.
    fn kernel<BuildType: FnOnce(LeafId) -> Result<LeafKernel, EvalError>>(
        &mut self,
        leaf: LeafId,
        build: BuildType,
    ) -> Result<usize, EvalError> {
        if let Some(kernel) = self.kernel_of_leaf.get(&leaf) {
            return Ok(*kernel);
        }
        self.kernels.push(build(leaf)?);
        let kernel = self.kernels.len() - 1;
        self.kernel_of_leaf.insert(leaf, kernel);
        Ok(kernel)
    }

    /// Appends a connective over `children`, which it orders cheapest first; its cost is that of
    /// its most expensive child.
    ///
    /// # Returns
    ///
    /// The appended node on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards the first error in `children`, as [`Self::kernel`] or [`Self::connective`]
    ///   returned it for a child.
    fn connective(
        &mut self,
        children: Vec<Result<Built, EvalError>>,
        is_and: bool,
    ) -> Result<Built, EvalError> {
        let mut children = children.into_iter().collect::<Result<Vec<_>, _>>()?;
        children.sort_by_key(|child| child.cost);
        let cost = children
            .iter()
            .map(|child| child.cost)
            .max()
            .unwrap_or(CostClass::Constant);
        let begin = self.children.len();
        self.children
            .extend(children.iter().map(|child| child.index));
        let node = Node::Connective {
            children: begin..self.children.len(),
            is_and,
        };
        Ok(self.push(node, cost))
    }
}
