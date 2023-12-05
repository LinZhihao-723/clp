#include "SchemaTree.hpp"

namespace ffi::ir_stream {
auto SchemaTree::get_node_with_id(size_t id) const -> SchemaTreeNode const& {
    if (m_tree_nodes.size() <= id) {
        throw SchemaTreeException(
                ErrorCode_OutOfBounds,
                __FILENAME__,
                __LINE__,
                "Schema tree id access out of bound."
        );
    }
    return m_tree_nodes[id];
}

auto SchemaTree::has_node(
        size_t parent_id,
        std::string_view key_name,
        SchemaTreeNodeValueType type,
        size_t& node_id
) const -> bool {
    auto const& parent{get_node_with_id(parent_id)};
    for (auto id : parent.get_children_ids()) {
        auto const& child{get_node_with_id(id)};
        if (child.get_key_name() != key_name || child.get_type() != type) {
            continue;
        }
        node_id = id;
        return true;
    }
    return false;
}

auto SchemaTree::try_insert_node(
        size_t parent_id,
        std::string_view key_name,
        SchemaTreeNodeValueType type,
        size_t& node_id
) -> bool {
    if (size_t existing_id{}; false != has_node(parent_id, key_name, type, existing_id)) {
        node_id = existing_id;
        return false;
    }
    if (SchemaTreeNodeValueType::Obj != m_tree_nodes[parent_id].get_type()) {
        throw SchemaTreeException(
                ErrorCode_BadParam,
                __FILENAME__,
                __LINE__,
                "Cannot insert a node to a leaf node."
        );
    }
    node_id = m_tree_nodes.size();
    m_tree_nodes.emplace_back(node_id, parent_id, key_name, type);
    m_tree_nodes[parent_id].add_child(node_id);
    return true;
}
}  // namespace ffi::ir_stream
