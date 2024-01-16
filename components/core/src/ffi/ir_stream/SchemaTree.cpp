#include "SchemaTree.hpp"

#include "encoding_methods.hpp"
#include "protocol_constants.hpp"

namespace ffi::ir_stream {
auto encoded_tag_to_tree_node_type(encoded_tag_t encoded_tag, SchemaTreeNodeValueType& type)
        -> bool {
    switch (encoded_tag) {
        case cProtocol::Payload::SchemaNodeInt:
            type = SchemaTreeNodeValueType::Int;
            break;
        case cProtocol::Payload::SchemaNodeFloat:
            type = SchemaTreeNodeValueType::Float;
            break;
        case cProtocol::Payload::SchemaNodeBool:
            type = SchemaTreeNodeValueType::Bool;
            break;
        case cProtocol::Payload::SchemaNodeStr:
            type = SchemaTreeNodeValueType::Str;
            break;
        case cProtocol::Payload::SchemaNodeObj:
            type = SchemaTreeNodeValueType::Obj;
            break;
        default:
            return false;
    }
    return true;
}

auto SchemaTreeNode::encode_as_new_node(std::vector<int8_t>& ir_buf) const -> bool {
    if (m_id < UINT8_MAX) {
        ir_buf.push_back(cProtocol::Payload::SchemaNodeParentIdByte);
        ir_buf.push_back(static_cast<uint8_t>(m_parent_id));
    } else if (m_id < UINT16_MAX) {
        ir_buf.push_back(cProtocol::Payload::SchemaNodeParentIdShort);
        encode_int(static_cast<uint16_t>(m_parent_id), ir_buf);
    } else {
        return false;
    }
    ir_buf.push_back(get_encoded_value_type_tag());
    auto const& name_length{m_key_name.length()};
    if (name_length < UINT8_MAX) {
        ir_buf.push_back(cProtocol::Payload::SchemaNodeNameLenByte);
        ir_buf.push_back(static_cast<uint8_t>(name_length));
    } else if (name_length < UINT16_MAX) {
        ir_buf.push_back(cProtocol::Payload::SchemaNodeNameLenShort);
        encode_int(static_cast<uint16_t>(name_length), ir_buf);
    } else {
        return false;
    }
    ir_buf.insert(ir_buf.cend(), m_key_name.cbegin(), m_key_name.cend());
    return true;
}

auto SchemaTreeNode::get_encoded_value_type_tag() const -> encoded_tag_t {
    switch (m_type) {
        case SchemaTreeNodeValueType::Int:
            return cProtocol::Payload::SchemaNodeInt;
        case SchemaTreeNodeValueType::Float:
            return cProtocol::Payload::SchemaNodeFloat;
        case SchemaTreeNodeValueType::Bool:
            return cProtocol::Payload::SchemaNodeBool;
        case SchemaTreeNodeValueType::Str:
            return cProtocol::Payload::SchemaNodeStr;
        case SchemaTreeNodeValueType::Obj:
            return cProtocol::Payload::SchemaNodeObj;
        default:
            return cProtocol::Payload::SchemaNodeUnknown;
    }
}

auto SchemaTreeNode::operator==(SchemaTreeNode const& tree_node) const -> bool {
    if (m_id != tree_node.m_id || m_parent_id != tree_node.m_parent_id || m_type != tree_node.m_type
        || m_key_name != tree_node.m_key_name)
    {
        return false;
    }
    if (m_children_ids.size() != tree_node.m_children_ids.size()) {
        return false;
    }
    for (size_t i{0}; i < tree_node.m_children_ids.size(); ++i) {
        if (m_children_ids[i] != tree_node.m_children_ids[i]) {
            return false;
        }
    }
    return true;
}

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

auto SchemaTree::dump() const -> std::string {
    std::string dump_result;
    dump_result += "id parent_id key_name type\n";
    for (auto it{m_tree_nodes.cbegin() + 1}; m_tree_nodes.cend() != it; ++it) {
        dump_result += it->dump();
    }
    return dump_result;
}

auto SchemaTree::operator==(SchemaTree const& tree) const -> bool {
    if (m_tree_nodes.size() != tree.m_tree_nodes.size()) {
        return false;
    }
    for (size_t i{0}; i < tree.m_tree_nodes.size(); ++i) {
        if (m_tree_nodes[i] == tree.m_tree_nodes[i]) {
            continue;
        }
        return false;
    }
    return true;
}
}  // namespace ffi::ir_stream
