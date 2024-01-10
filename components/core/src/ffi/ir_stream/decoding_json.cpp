#include "decoding_json.hpp"

#include <iostream>

#include "protocol_constants.hpp"
#include "Values.hpp"

namespace ffi::ir_stream {
namespace {
    /**
     * Growths the schema tree.
     * @param encoded_tag
     * @param reader
     * @param schema_tree
     * @return IRErrorCode_Success on success; related error code otherwise.
     */
    [[nodiscard]] auto
    schema_tree_growth(encoded_tag_t encoded_tag, ReaderInterface& reader, SchemaTree& schema_tree)
            -> IRErrorCode {
        size_t parent_id{};
        if (cProtocol::Payload::SchemaNodeParentIdByte == encoded_tag) {
            uint8_t decoded_id{};
            if (false == decode_int(reader, decoded_id)) {
                return IRErrorCode::IRErrorCode_Incomplete_IR;
            }
            parent_id = static_cast<size_t>(decoded_id);
        } else if (cProtocol::Payload::SchemaNodeParentIdShort == encoded_tag) {
            uint16_t decoded_id{};
            if (false == decode_int(reader, decoded_id)) {
                return IRErrorCode::IRErrorCode_Incomplete_IR;
            }
            parent_id = static_cast<size_t>(decoded_id);
        } else {
            return IRErrorCode::IRErrorCode_Corrupted_IR;
        }

        encoded_tag_t type_tag;
        if (ErrorCode_Success != reader.try_read_numeric_value(type_tag)) {
            return IRErrorCode_Incomplete_IR;
        }
        SchemaTreeNodeValueType type{SchemaTreeNodeValueType::Unknown};
        if (false == encoded_tag_to_tree_node_type(type_tag, type)) {
            return IRErrorCode::IRErrorCode_Corrupted_IR;
        }

        encoded_tag_t len_tag;
        if (ErrorCode_Success != reader.try_read_numeric_value(len_tag)) {
            return IRErrorCode_Incomplete_IR;
        }
        size_t name_len;
        if (cProtocol::Payload::SchemaNodeNameLenByte == len_tag) {
            uint8_t decoded_len{};
            if (false == decode_int(reader, decoded_len)) {
                return IRErrorCode::IRErrorCode_Incomplete_IR;
            }
            name_len = static_cast<size_t>(decoded_len);
        } else if (cProtocol::Payload::SchemaNodeParentIdShort == len_tag) {
            uint16_t decoded_len{};
            if (false == decode_int(reader, decoded_len)) {
                return IRErrorCode::IRErrorCode_Incomplete_IR;
            }
            name_len = static_cast<size_t>(decoded_len);
        } else {
            return IRErrorCode::IRErrorCode_Corrupted_IR;
        }
        std::string node_name;
        if (ErrorCode_Success != reader.try_read_string(name_len, node_name)) {
            return IRErrorCode::IRErrorCode_Incomplete_IR;
        }

        size_t new_node_id;
        if (false == schema_tree.try_insert_node(parent_id, node_name, type, new_node_id)) {
            return IRErrorCode::IRErrorCode_Decode_Error;
        }

        return IRErrorCode_Success;
    }

    /**
     * Deserializes the next key value pair from the reader.
     * @param tag
     * @param reader
     * @param schema_tree This tree will be used to validate the deserialized
     * value type.
     * @param decoded_tree_node_id
     * @param decoded_value
     * @return IRErrorCode_Success on success; related error code otherwise.
     */
    [[nodiscard]] auto deserialize_key_value_pair(
            encoded_tag_t tag,
            ReaderInterface& reader,
            SchemaTree const& schema_tree,
            size_t& decoded_tree_node_id,
            Value& decoded_value
    ) -> IRErrorCode {
        if (cProtocol::Payload::SchemaNodeIdByte == tag) {
            uint8_t tree_node_id{};
            if (false == decode_int(reader, tree_node_id)) {
                return IRErrorCode::IRErrorCode_Incomplete_IR;
            }
            decoded_tree_node_id = static_cast<size_t>(tree_node_id);
        } else if (cProtocol::Payload::SchemaNodeIdShort == tag) {
            uint16_t tree_node_id{};
            if (false == decode_int(reader, tree_node_id)) {
                return IRErrorCode::IRErrorCode_Incomplete_IR;
            }
            decoded_tree_node_id = static_cast<size_t>(tree_node_id);
        } else {
            return IRErrorCode::IRErrorCode_Corrupted_IR;
        }

        if (auto const error{decoded_value.decode_from_reader(reader)};
            IRErrorCode::IRErrorCode_Success != error)
        {
            return error;
        }
        if (decoded_value.get_schema_tree_node_type()
            != schema_tree.get_node_with_id(decoded_tree_node_id).get_type())
        {
            return IRErrorCode::IRErrorCode_Decode_Error;
        }
        return IRErrorCode_Success;
    }

    /**
     * Inserts the decoded key value pair into the json object.
     * @param decoded_id
     * @param decoded_value
     * @param schema_tree
     * @param keys_to_root
     * @param obj The json object to insert the key value pair.
     * @return IRErrorCode_Success on success; related error code otherwise.
     */
    [[nodiscard]] auto insert_key_value_pair(
            size_t decoded_id,
            Value const& decoded_value,
            SchemaTree const& schema_tree,
            std::vector<std::string_view>& keys_to_root,
            nlohmann::json& obj
    ) -> IRErrorCode {
        keys_to_root.clear();
        size_t curr_id{decoded_id};
        nlohmann::json* obj_ptr{&obj};
        while (SchemaTree::cRootId != curr_id) {
            auto const& node{schema_tree.get_node_with_id(curr_id)};
            keys_to_root.push_back(node.get_key_name());
            curr_id = node.get_parent_id();
        }
        for (size_t i{keys_to_root.size() - 1}; 0 < i; --i) {
            auto it{obj_ptr->find(keys_to_root[i])};
            if (obj_ptr->end() == it) {
                auto [inserted_it,
                      inserted]{obj_ptr->emplace(keys_to_root[i], nlohmann::json::object())};
                it = inserted_it;
            }
            obj_ptr = &(*it);
        }
        std::string_view base_key{keys_to_root[0]};
        switch (schema_tree.get_node_with_id(decoded_id).get_type()) {
            case SchemaTreeNodeValueType::Int:
                obj_ptr->emplace(base_key, decoded_value.get<value_int_t>());
                break;
            case SchemaTreeNodeValueType::Float:
                obj_ptr->emplace(base_key, decoded_value.get<value_float_t>());
                break;
            case SchemaTreeNodeValueType::Bool:
                obj_ptr->emplace(base_key, decoded_value.get<value_bool_t>());
                break;
            case SchemaTreeNodeValueType::Str:
                obj_ptr->emplace(
                        base_key,
                        static_cast<std::string>(decoded_value.get<value_str_t>())
                );
                break;
            case SchemaTreeNodeValueType::Obj:
                if (false == decoded_value.is_empty()) {
                    return IRErrorCode::IRErrorCode_Decode_Error;
                }
                obj_ptr->emplace(base_key, nullptr);
                break;
            default:
                return IRErrorCode::IRErrorCode_Decode_Error;
        }
        return IRErrorCode::IRErrorCode_Success;
    }

    /**
     * Deserializes the next key value pair record. It could contain multiple
     * key value pairs.
     * @param tag
     * @param reader
     * @param schema_tree
     * @param obj The deserialized json object.
     * @return IRErrorCode_Success on success; related error code otherwise.
     */
    [[nodiscard]] auto deserialize_key_value_pair_record(
            encoded_tag_t tag,
            ReaderInterface& reader,
            SchemaTree const& schema_tree,
            nlohmann::json& obj
    ) -> IRErrorCode {
        Value value;
        size_t id{};
        std::vector<std::string_view> keys_to_root;
        obj = nlohmann::json::object();
        while (true) {
            if (cProtocol::Payload::KeyValuePairRecordDeliminator == tag) {
                break;
            }
            if (auto const error{deserialize_key_value_pair(tag, reader, schema_tree, id, value)};
                IRErrorCode::IRErrorCode_Success != error)
            {
                return error;
            }
            if (auto const error{insert_key_value_pair(id, value, schema_tree, keys_to_root, obj)};
                IRErrorCode::IRErrorCode_Success != error)
            {
                return error;
            }
            if (ErrorCode_Success != reader.try_read_numeric_value(tag)) {
                return IRErrorCode_Incomplete_IR;
            }
        }
        return IRErrorCode_Success;
    }
}  // namespace

auto decode_json_object(ReaderInterface& reader, SchemaTree& schema_tree, nlohmann::json& obj)
        -> IRErrorCode {
    encoded_tag_t encoded_tag{cProtocol::Eof};

    // Schema tree growth
    while (true) {
        if (ErrorCode_Success != reader.try_read_numeric_value(encoded_tag)) {
            return IRErrorCode_Incomplete_IR;
        }
        if (cProtocol::Eof == encoded_tag) {
            return IRErrorCode_Eof;
        }
        if (cProtocol::Payload::SchemaNodeParentIdByte != encoded_tag
            && cProtocol::Payload::SchemaNodeParentIdShort != encoded_tag)
        {
            break;
        }
        if (auto const error{schema_tree_growth(encoded_tag, reader, schema_tree)};
            IRErrorCode::IRErrorCode_Success != error)
        {
            return error;
        }
    }

    if (cProtocol::Payload::SchemaNodeIdByte == encoded_tag
        || cProtocol::Payload::SchemaNodeIdShort == encoded_tag)
    {
        return deserialize_key_value_pair_record(encoded_tag, reader, schema_tree, obj);
    }

    return IRErrorCode::IRErrorCode_Decode_Error;
}
}  // namespace ffi::ir_stream
