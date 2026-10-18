//! The column resolver: binds column references to schema-tree nodes, once per distinct
//! reference, in filter mode (the search semantics) or projection mode (the C++ projection rules).

use std::collections::HashSet;

use clp_s_format::NodeId;
use clp_s_format::NodeType;
use clp_s_format::SchemaTree;
use clp_s_format::TimestampIndex;
use clp_s_query::ColumnPattern;
use clp_s_query::ColumnRef;
use clp_s_query::Namespace;
use clp_s_query::PathToken;

/// How the values below an unstructured array are reached: the reference evaluator's walk applied
/// to the array value, per row.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ArrayAccess {
    /// The values `tokens` reach from the array (arrays are transparent at every step; an empty
    /// suffix reaches the elements, recursively, and an empty array reaches itself as an empty
    /// container).
    Walk(Box<[PathToken]>),

    /// The values `tokens` reach from the array once the leading `*` of `tokens` has consumed at
    /// least one level: the remaining tokens, or the `*` consuming more levels.
    BelowWildcard(Box<[PathToken]>),

    /// Every scalar at any depth (bare values).
    AnyLeaf,
}

/// What a resolved node supplies.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NodeAccess {
    /// The node's own value (a column, a null, or an object).
    Value,

    /// Values inside the node's unstructured array.
    Array(ArrayAccess),
}

/// One node a column reference reaches, with how it reaches it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeBinding {
    pub node: NodeId,
    pub access: NodeAccess,
}

/// Resolves column references against one archive's schema tree.
pub struct ColumnResolver<'tree> {
    tree: &'tree SchemaTree,
    timestamps: &'tree TimestampIndex,
}

impl<'tree> ColumnResolver<'tree> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The resolver over `tree`, whose authoritative timestamp node comes from `timestamps`.
    pub const fn new(tree: &'tree SchemaTree, timestamps: &'tree TimestampIndex) -> Self {
        Self { tree, timestamps }
    }

    /// Resolves a filter column (search semantics, `docs/semantics.md` §2): keys descend into
    /// objects, `*` matches zero or more object levels, arrays are transparent at every step, and
    /// bare values reach every non-metadata leaf of every namespace.
    ///
    /// Range-index fields resolve to no node; they are evaluated against the range index.
    ///
    /// # Returns
    ///
    /// The distinct node bindings, in discovery order.
    pub fn resolve_filter(&self, column: &ColumnRef) -> Vec<NodeBinding> {
        match column {
            ColumnRef::Path(pattern) => self.resolve_path(pattern),
            ColumnRef::AnyLeaf => self.resolve_any_leaf(),
            ColumnRef::AuthoritativeTimestamp => self
                .timestamps
                .authoritative()
                .filter(|entry| {
                    self.tree
                        .node(entry.node_id)
                        .is_some_and(|node| NodeType::Timestamp == node.node_type)
                })
                .map(|entry| NodeBinding {
                    node: entry.node_id,
                    access: NodeAccess::Value,
                })
                .into_iter()
                .collect(),
            ColumnRef::RangeIndexField(_) => Vec::new(),
        }
    }

    /// Resolves a projection column with the C++ rules (`search/Projection.cpp`): a literal path
    /// whose intermediate tokens are objects and whose last token names leaf nodes of any literal
    /// type (never objects).
    ///
    /// # Returns
    ///
    /// The nodes of the column, in id order.
    pub fn resolve_projection(&self, pattern: &ColumnPattern) -> Vec<NodeId> {
        let Some(mut current) = self.namespace_root(pattern.namespace) else {
            return Vec::new();
        };
        let mut nodes = Vec::new();
        for (index, token) in pattern.tokens.iter().enumerate() {
            let PathToken::Key(key) = token else {
                return Vec::new();
            };
            let is_last_token = index + 1 == pattern.tokens.len();
            let mut next = None;
            for child in self.tree.children(current) {
                let Some(child_node) = self.tree.node(*child) else {
                    continue;
                };
                if child_node.key != &**key {
                    continue;
                }
                if is_last_token {
                    if is_projectable_leaf(child_node.node_type) {
                        nodes.push(*child);
                    }
                } else if NodeType::Object == child_node.node_type {
                    next = Some(*child);
                    break;
                }
            }
            if is_last_token {
                break;
            }
            let Some(next) = next else {
                break;
            };
            current = next;
        }
        nodes.sort_unstable();
        nodes
    }

    /// # Returns
    ///
    /// The object root of `namespace`, or `None` if the archive has none.
    fn namespace_root(&self, namespace: Namespace) -> Option<NodeId> {
        let key: &[u8] = match namespace {
            Namespace::Default => b"",
            Namespace::AutoGenerated => b"@",
            _ => return None,
        };
        self.tree.subtree_root(key, NodeType::Object)
    }

    /// Walks `pattern` over the schema tree, iteratively, visiting each `(node, token position,
    /// mode)` state once.
    ///
    /// # Returns
    ///
    /// The distinct bindings of `pattern`.
    fn resolve_path(&self, pattern: &ColumnPattern) -> Vec<NodeBinding> {
        let Some(root) = self.namespace_root(pattern.namespace) else {
            return Vec::new();
        };
        let tokens = &*pattern.tokens;
        let mut bindings = Bindings::default();
        let mut visited: HashSet<(NodeId, usize, bool)> = HashSet::new();
        let mut stack = vec![(root, 0_usize, false)];
        while let Some(state) = stack.pop() {
            if !visited.insert(state) {
                continue;
            }
            let (node, position, is_below_wildcard) = state;
            let Some(node_type) = self.tree.node(node).map(|view| view.node_type) else {
                continue;
            };
            if is_below_wildcard {
                if NodeType::UnstructuredArray == node_type {
                    bindings.push(
                        node,
                        NodeAccess::Array(ArrayAccess::BelowWildcard(tokens[position..].into())),
                    );
                    continue;
                }
                stack.push((node, position + 1, false));
                if NodeType::Object == node_type {
                    stack.extend(
                        self.tree
                            .children(node)
                            .iter()
                            .map(|child| (*child, position, true)),
                    );
                }
                continue;
            }
            let Some(token) = tokens.get(position) else {
                let access = if NodeType::UnstructuredArray == node_type {
                    NodeAccess::Array(ArrayAccess::Walk(Box::new([])))
                } else {
                    NodeAccess::Value
                };
                bindings.push(node, access);
                continue;
            };
            match node_type {
                NodeType::UnstructuredArray => bindings.push(
                    node,
                    NodeAccess::Array(ArrayAccess::Walk(tokens[position..].into())),
                ),
                NodeType::Object => match token {
                    PathToken::AnyDepth => {
                        stack.push((node, position + 1, false));
                        stack.extend(
                            self.tree
                                .children(node)
                                .iter()
                                .map(|child| (*child, position, true)),
                        );
                    }
                    PathToken::Key(key) => {
                        for child in self.tree.children(node) {
                            if self
                                .tree
                                .node(*child)
                                .is_some_and(|view| view.key == &**key)
                            {
                                stack.push((*child, position + 1, false));
                            }
                        }
                    }
                },
                _ => {}
            }
        }
        bindings.into_vec()
    }

    /// # Returns
    ///
    /// Every leaf of every object namespace (never the metadata subtree), as bare values reach
    /// them: scalars and nulls as values, unstructured arrays through their elements.
    fn resolve_any_leaf(&self) -> Vec<NodeBinding> {
        let mut bindings = Vec::new();
        let num_nodes = u32::try_from(self.tree.len())
            .expect("the format layer bounds the node count by i32::MAX");
        let roots: Vec<NodeId> = (0..num_nodes)
            .map(NodeId::new)
            .filter(|node| {
                self.tree
                    .node(*node)
                    .is_some_and(|view| view.parent.is_none() && NodeType::Object == view.node_type)
            })
            .collect();
        let mut stack = roots;
        while let Some(node) = stack.pop() {
            let Some(node_type) = self.tree.node(node).map(|view| view.node_type) else {
                continue;
            };
            match node_type {
                NodeType::Object => stack.extend(self.tree.children(node).iter().copied()),
                NodeType::UnstructuredArray => bindings.push(NodeBinding {
                    node,
                    access: NodeAccess::Array(ArrayAccess::AnyLeaf),
                }),
                NodeType::Metadata | NodeType::StructuredArray => {}
                _ => bindings.push(NodeBinding {
                    node,
                    access: NodeAccess::Value,
                }),
            }
        }
        bindings.sort_by_key(|binding| binding.node);
        bindings
    }
}

/// An insertion-ordered set of bindings.
#[derive(Default)]
struct Bindings {
    seen: HashSet<NodeBinding>,
    ordered: Vec<NodeBinding>,
}

impl Bindings {
    /// Adds the binding of `node` through `access` unless it is already present.
    fn push(&mut self, node: NodeId, access: NodeAccess) {
        let binding = NodeBinding { node, access };
        if self.seen.insert(binding.clone()) {
            self.ordered.push(binding);
        }
    }

    /// # Returns
    ///
    /// The bindings in insertion order.
    fn into_vec(self) -> Vec<NodeBinding> {
        self.ordered
    }
}

/// # Returns
///
/// Whether a node of `node_type` has a literal type a projection can select (C++
/// `node_to_literal_type` is not `UnknownT`).
const fn is_projectable_leaf(node_type: NodeType) -> bool {
    !matches!(
        node_type,
        NodeType::Object | NodeType::Metadata | NodeType::StructuredArray
    )
}
