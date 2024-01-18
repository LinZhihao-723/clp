#include "encoding_json.hpp"

#include <iostream>

#include "encoding_methods.hpp"
#include "protocol_constants.hpp"
#include "Values.hpp"

namespace ffi::ir_stream {
namespace {
    using JsonObjectIterator = decltype(std::declval<nlohmann::json const>().items().begin());

    struct DfsStackNode {
        DfsStackNode(JsonObjectIterator b, JsonObjectIterator e, size_t id)
                : begin(b),
                  end(e),
                  schema_tree_node_id{id} {}

        JsonObjectIterator begin;
        JsonObjectIterator end;
        size_t schema_tree_node_id;
    };

    auto encode_schema_id(size_t id, std::vector<int8_t>& ir_buf) -> bool {
        if (id < UINT16_MAX) {
            ir_buf.push_back(cProtocol::Payload::SchemaNodeIdShort);
            encode_int(static_cast<uint16_t>(id), ir_buf);
        } else {
            return false;
        }
        return true;
    }

    auto get_value_type_from_json(nlohmann::json const& value) -> SchemaTreeNodeValueType {
        auto type{SchemaTreeNodeValueType::Unknown};
        if (value.is_number_integer()) {
            type = SchemaTreeNodeValueType::Int;
        } else if (value.is_number_float()) {
            type = SchemaTreeNodeValueType::Float;
        } else if (value.is_boolean()) {
            type = SchemaTreeNodeValueType::Bool;
        } else if (value.is_string()) {
            type = SchemaTreeNodeValueType::Str;
        } else if (value.is_null()) {
            type = SchemaTreeNodeValueType::Obj;
        }
        return type;
    }

    auto serialize_json_array(
            nlohmann::json const& json_array,
            SchemaTree& schema_tree,
            std::vector<int8_t>& ir_buf,
            std::vector<size_t>& inserted_schema_tree_node_ids
    ) -> bool;

    auto serialize_json_object(
            nlohmann::json const& json_array,
            SchemaTree& schema_tree,
            std::vector<int8_t>& ir_buf,
            std::vector<size_t>& inserted_schema_tree_node_ids
    ) -> bool;

    auto get_schema_node_id(
            SchemaTree& schema_tree,
            size_t parent_id,
            SchemaTreeNodeValueType type,
            std::string_view key,
            std::vector<size_t>& inserted_schema_tree_node_ids
    ) -> size_t {
        size_t node_id{};
        auto const inserted{schema_tree.try_insert_node(parent_id, key, type, node_id)};
        if (inserted) {
            inserted_schema_tree_node_ids.push_back(node_id);
        }
        return node_id;
    }

    auto serialize_json_array(
            nlohmann::json const& json_array,
            SchemaTree& schema_tree,
            std::vector<int8_t>& ir_buf,
            std::vector<size_t>& inserted_schema_tree_node_ids
    ) -> bool {
        if (false == json_array.is_array()) {
            return false;
        }
        if (json_array.empty()) {
            ir_buf.push_back(cProtocol::Payload::EmptyArray);
            return true;
        }
        ir_buf.push_back(cProtocol::Payload::ArrayBegin);
        for (auto const& item : json_array) {
            if (false == item.is_object() && false == item.is_array()) {
                // Single value encoding.
                auto const type{get_value_type_from_json(item)};
                if (type == SchemaTreeNodeValueType::Unknown) {
                    return false;
                }
                auto const value_from_json{Value::convert_from_json(type, item)};
                if (false == value_from_json.encode(ir_buf)) {
                    return false;
                }
                continue;
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
            nlohmann::json const& root,
            SchemaTree& schema_tree,
            std::vector<int8_t>& ir_buf,
            std::vector<size_t>& inserted_schema_tree_node_ids
    ) -> bool {
        if (root.is_array()) {
            return serialize_json_array(root, schema_tree, ir_buf, inserted_schema_tree_node_ids);
        }
        if (false == root.is_object()) {
            return false;
        }
        if (root.empty()) {
            ir_buf.push_back(cProtocol::Payload::EmptyObj);
            return true;
        }
        std::vector<DfsStackNode> working_stack;
        working_stack.emplace_back(root.items().begin(), root.items().end(), SchemaTree::cRootId);
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
                auto const node_id{get_schema_node_id(
                        schema_tree,
                        parent_id,
                        SchemaTreeNodeValueType::Obj,
                        key,
                        inserted_schema_tree_node_ids
                )};
                if (false == encode_schema_id(node_id, ir_buf)) {
                    return false;
                }
                if (false
                    == serialize_json_array(
                            value,
                            schema_tree,
                            ir_buf,
                            inserted_schema_tree_node_ids
                    ))
                {
                    return false;
                }
                continue;
            }
            if (value.is_object()) {
                if (value.empty()) {
                    auto const node_id{get_schema_node_id(
                            schema_tree,
                            parent_id,
                            SchemaTreeNodeValueType::Obj,
                            key,
                            inserted_schema_tree_node_ids
                    )};
                    if (false == encode_schema_id(node_id, ir_buf)) {
                        return false;
                    }
                    ir_buf.push_back(cProtocol::Payload::EmptyObj);
                } else {
                    auto const node_id{get_schema_node_id(
                            schema_tree,
                            parent_id,
                            SchemaTreeNodeValueType::Obj,
                            key,
                            inserted_schema_tree_node_ids
                    )};
                    working_stack.emplace_back(value.items().begin(), value.items().end(), node_id);
                }
                continue;
            }

            auto const type{get_value_type_from_json(value)};
            if (type == SchemaTreeNodeValueType::Unknown) {
                return false;
            }
            auto const value_from_json{Value::convert_from_json(type, value)};
            auto const node_id{get_schema_node_id(
                    schema_tree,
                    parent_id,
                    type,
                    key,
                    inserted_schema_tree_node_ids
            )};
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
}  // namespace

auto encode_json_object(
        nlohmann::json const& json,
        SchemaTree& schema_tree,
        std::vector<int8_t>& ir_buf
) -> bool {
    std::vector<int8_t> encoded_json_record;
    std::vector<size_t> inserted_tree_node_ids;
    schema_tree.snapshot();
    if (false
        == serialize_json_object(json, schema_tree, encoded_json_record, inserted_tree_node_ids))
    {
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
