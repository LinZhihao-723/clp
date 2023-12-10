#include "encoding_json.hpp"

#include "encoding_methods.hpp"
#include "protocol_constants.hpp"
#include "Values.hpp"

namespace ffi::ir_stream {
namespace {
    template <typename JsonObjectIterator>
    struct DfsStackNode {
        DfsStackNode(JsonObjectIterator b, JsonObjectIterator e, size_t id)
                : begin{b},
                  end{e},
                  schema_tree_node_id{id} {}

        JsonObjectIterator begin;
        JsonObjectIterator end;
        size_t schema_tree_node_id;
    };

    auto encode_schema_id(size_t id, std::vector<int8_t>& ir_buf) -> bool {
        if (id < UINT8_MAX) {
            ir_buf.push_back(cProtocol::Payload::SchemaNodeIdByte);
            ir_buf.push_back(static_cast<uint8_t>(id));
        } else if (id < UINT16_MAX) {
            ir_buf.push_back(cProtocol::Payload::SchemaNodeIdShort);
            encode_int(static_cast<uint16_t>(id), ir_buf);
        } else {
            return false;
        }
        return true;
    }
}  // namespace

auto serialize_json_array(
        nlohmann::json const& json_array,
        SchemaTree& schema_tree,
        std::vector<int8_t>& ir_buf,
        std::vector<size_t>& inserted_schema_tree_node_ids
) -> bool {
    ir_buf.push_back(cProtocol::Payload::ArrayBegin);

    if (false == json_array.is_array()) {
        return false;
    }
    for (auto const& item : json_array) {
        if (false == item.is_object() || false == item.is_array()) {
            // TODO: currently, we don't support raw values in an array.
            return false;
        }
        if (false
            == serialize_json_object(item, schema_tree, ir_buf, inserted_schema_tree_node_ids))
        {
            return false;
        }
    }

    ir_buf.push_back(cProtocol::Payload::ArrayEnd);
    return true;
}

auto serialize_json_object(
        nlohmann::json const& json_obj,
        SchemaTree& schema_tree,
        std::vector<int8_t>& ir_buf,
        std::vector<size_t>& inserted_schema_tree_node_ids
) -> bool {
    if (json_obj.is_array()) {
        return serialize_json_object(json_obj, schema_tree, ir_buf, inserted_schema_tree_node_ids);
    }
    if (false == json_obj.is_object()) {
        return false;
    }
    std::vector<DfsStackNode<nlohmann::detail::iter_impl<nlohmann::json const>>> working_stack;
    working_stack
            .emplace_back(json_obj.items().begin(), json_obj.items().end(), SchemaTree::cRootId);
    while (false == working_stack.empty()) {
        auto& [it_curr, it_end, parent_id]{working_stack.back()};
        if (it_curr == it_end) {
            working_stack.pop_back();
            continue;
        }
        std::string_view key{it_curr.key()};
        auto const& value{it_curr.value()};
        ++it_curr;

        if (value.is_array()) {
            size_t node_id{};
            auto const inserted{
                    schema_tree
                            .try_insert_node(parent_id, key, SchemaTreeNodeValueType::Obj, node_id)
            };
            if (inserted) {
                inserted_schema_tree_node_ids.push_back(node_id);
            }
            if (false == encode_schema_id(node_id, ir_buf)) {
                return false;
            }
            if (false
                == serialize_json_array(value, schema_tree, ir_buf, inserted_schema_tree_node_ids))
            {
                return false;
            }
            continue;
        } else if (value.is_object()) {
            size_t node_id{};
            auto const inserted{
                    schema_tree
                            .try_insert_node(parent_id, key, SchemaTreeNodeValueType::Obj, node_id)
            };
            if (inserted) {
                inserted_schema_tree_node_ids.push_back(node_id);
            }
            working_stack.emplace_back(value.items().begin(), value.items().end(), node_id);
            continue;
        }

        SchemaTreeNodeValueType type{SchemaTreeNodeValueType::Unknown};
        if (value.is_number_integer()) {
            type = SchemaTreeNodeValueType::Int;
        } else if (value.is_number_float()) {
            type = SchemaTreeNodeValueType::Float;
        } else if (value.is_boolean()) {
            type = SchemaTreeNodeValueType::Bool;
        } else if (value.is_string()) {
            type = SchemaTreeNodeValueType::Str;
        } else if (value.is_null()) {
            type == SchemaTreeNodeValueType::Obj;
        }
        if (type == SchemaTreeNodeValueType::Unknown) {
            return false;
        }
        size_t node_id{};
        auto const inserted{schema_tree.try_insert_node(parent_id, key, type, node_id)};
        if (inserted) {
            inserted_schema_tree_node_ids.push_back(node_id);
        }
        auto const value_from_json{Value::convert_from_json(type, value)};
        if (false == encode_schema_id(node_id, ir_buf)) {
            return false;
        }
        if (false == value_from_json.encode(ir_buf)) {
            return false;
        }
    }

    ir_buf.push_back(cProtocol::Payload::KeyValuePairRecordDeliminator);
    return true;
}

auto encode_json_object(
        nlohmann::json const& json,
        SchemaTree& schema_tree,
        std::vector<int8_t>& ir_buf
) -> bool {
    ir_buf.clear();
    std::vector<int8_t> encoded_json_record;
    std::vector<size_t> inserted_tree_node_ids;
    schema_tree.snapshot();
    if (false == serialize_json_object(json, schema_tree, ir_buf, inserted_tree_node_ids)) {
        schema_tree.revert();
        return false;
    }
    for (auto const id : inserted_tree_node_ids) {
        auto const& node{schema_tree.get_node_with_id(id)};
        if (node.encode_as_new_node(ir_buf)) {
            continue;
        }
        schema_tree.revert();
        return false;
    }
    ir_buf.insert(ir_buf.cend(), encoded_json_record.cbegin(), encoded_json_record.cend());
    return true;
}
}  // namespace ffi::ir_stream
