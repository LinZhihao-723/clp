#include "decoding_json.hpp"

#include <iostream>

#include "protocol_constants.hpp"
#include "Values.hpp"

namespace ffi::ir_stream {
namespace {
    /**
     * @return true if the encoded tag is a key value pair.
     */
    [[nodiscard]] auto is_encoded_key_value_pair_tag(encoded_tag_t tag) -> bool {
        return cProtocol::Payload::SchemaNodeIdByte == tag
               || cProtocol::Payload::SchemaNodeIdShort == tag;
    }

    [[nodiscard]] auto is_array_tag(encoded_tag_t tag) -> bool {
        return cProtocol::Payload::ArrayBegin == tag;
    }

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
     * Deserializes the key id from the reader.
     * @param tag
     * @param reader
     * @param key_id
     * @param decoded_value
     * @return IRErrorCode_Success on success; related error code otherwise.
     */
    [[nodiscard]] auto
    deserialize_key_id(encoded_tag_t tag, ReaderInterface& reader, size_t& key_id) -> IRErrorCode {
        if (cProtocol::Payload::SchemaNodeIdByte == tag) {
            uint8_t tree_node_id{};
            if (false == decode_int(reader, tree_node_id)) {
                return IRErrorCode::IRErrorCode_Incomplete_IR;
            }
            key_id = static_cast<size_t>(tree_node_id);
        } else if (cProtocol::Payload::SchemaNodeIdShort == tag) {
            uint16_t tree_node_id{};
            if (false == decode_int(reader, tree_node_id)) {
                return IRErrorCode::IRErrorCode_Incomplete_IR;
            }
            key_id = static_cast<size_t>(tree_node_id);
        } else {
            return IRErrorCode::IRErrorCode_Corrupted_IR;
        }
        return IRErrorCode::IRErrorCode_Success;
    }

    /**
     * Deserializes the next value from the reader.
     * @param tag
     * @param reader
     * @param schema_tree This tree will be used to validate the deserialized
     * value type.
     * @param key_id
     * @param value
     * @return IRErrorCode_Success on success; related error code otherwise.
     */
    [[nodiscard]] auto deserialize_value(
            encoded_tag_t tag,
            ReaderInterface& reader,
            SchemaTree const& schema_tree,
            size_t key_id,
            Value& value
    ) -> IRErrorCode {
        if (auto const error{value.decode_from_reader(reader, tag)};
            IRErrorCode::IRErrorCode_Success != error)
        {
            return error;
        }
        if (value.get_schema_tree_node_type() != schema_tree.get_node_with_id(key_id).get_type()) {
            return IRErrorCode::IRErrorCode_Decode_Error;
        }
        return IRErrorCode::IRErrorCode_Success;
    }

    /**
     * Inserts the decoded key value pair into the json object.
     * @tparam ValueType The type of the value.
     * @param decoded_id
     * @param value
     * @param schema_tree
     * @param keys_to_root
     * @param obj The json object to insert the key value pair.
     */
    template <typename ValueType>
    [[nodiscard]] auto insert_key_value_pair(
            size_t decoded_id,
            ValueType const& value,
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
        obj_ptr->emplace(keys_to_root[0], value);
    }

    /**
     * Deserializes an encoded json array.
     * @param tag
     * @param reader
     * @param schema_tree
     * @param array The deserialized array.
     * @return IRErrorCode_Success on success; related error code otherwise.
     */
    [[nodiscard]] auto deserialize_array(
            encoded_tag_t tag,
            ReaderInterface& reader,
            SchemaTree const& schema_tree,
            nlohmann::json& array
    ) -> IRErrorCode;

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
    ) -> IRErrorCode;

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
            if (auto const error{deserialize_key_id(tag, reader, id)};
                IRErrorCode::IRErrorCode_Success != error)
            {
                return error;
            }
            if (ErrorCode_Success != reader.try_read_numeric_value(tag)) {
                return IRErrorCode::IRErrorCode_Incomplete_IR;
            }
            if (is_array_tag(tag)) {
                nlohmann::json sub_array;
                if (auto const error{deserialize_array(tag, reader, schema_tree, sub_array)};
                    IRErrorCode::IRErrorCode_Success != error)
                {
                    return error;
                }
                if (schema_tree.get_node_with_id(id).get_type() != SchemaTreeNodeValueType::Obj) {
                    return IRErrorCode::IRErrorCode_Decode_Error;
                }
                insert_key_value_pair(id, sub_array, schema_tree, keys_to_root, obj);
            } else {
                if (auto const error{deserialize_value(tag, reader, schema_tree, id, value)};
                    IRErrorCode::IRErrorCode_Success != error)
                {
                    return error;
                }
                switch (schema_tree.get_node_with_id(id).get_type()) {
                    case SchemaTreeNodeValueType::Int:
                        insert_key_value_pair(
                                id,
                                value.get<value_int_t>(),
                                schema_tree,
                                keys_to_root,
                                obj
                        );
                        break;
                    case SchemaTreeNodeValueType::Float:
                        insert_key_value_pair(
                                id,
                                value.get<value_float_t>(),
                                schema_tree,
                                keys_to_root,
                                obj
                        );
                        break;
                    case SchemaTreeNodeValueType::Bool:
                        insert_key_value_pair(
                                id,
                                value.get<value_bool_t>(),
                                schema_tree,
                                keys_to_root,
                                obj
                        );
                        break;
                    case SchemaTreeNodeValueType::Str:
                        insert_key_value_pair(
                                id,
                                static_cast<std::string>(value.get<value_str_t>()),
                                schema_tree,
                                keys_to_root,
                                obj
                        );
                        break;
                    case SchemaTreeNodeValueType::Obj:
                        if (false == value.is_empty()) {
                            return IRErrorCode::IRErrorCode_Decode_Error;
                        }
                        insert_key_value_pair(id, nullptr, schema_tree, keys_to_root, obj);
                        break;
                    default:
                        return IRErrorCode::IRErrorCode_Decode_Error;
                }
            }
            if (ErrorCode_Success != reader.try_read_numeric_value(tag)) {
                return IRErrorCode_Incomplete_IR;
            }
        }
        return IRErrorCode_Success;
    }

    [[nodiscard]] auto deserialize_array(
            encoded_tag_t tag,
            ReaderInterface& reader,
            SchemaTree const& schema_tree,
            nlohmann::json& array
    ) -> IRErrorCode {
        if (false == is_array_tag(tag)) {
            return IRErrorCode_Corrupted_IR;
        }
        array = nlohmann::json::array();
        while (true) {
            if (ErrorCode_Success != reader.try_read_numeric_value(tag)) {
                return IRErrorCode_Incomplete_IR;
            }
            if (cProtocol::Payload::ArrayEnd == tag) {
                break;
            }

            if (is_array_tag(tag)) {
                nlohmann::json sub_array;
                if (auto const err{deserialize_array(tag, reader, schema_tree, sub_array)};
                    IRErrorCode::IRErrorCode_Success != err)
                {
                    return err;
                }
                array.push_back(sub_array);
            } else if (is_encoded_key_value_pair_tag(tag)) {
                nlohmann::json key_value_pair_record;
                if (auto const err{deserialize_key_value_pair_record(
                            tag,
                            reader,
                            schema_tree,
                            key_value_pair_record
                    )};
                    IRErrorCode::IRErrorCode_Success != err)
                {
                    return err;
                }
                array.push_back(key_value_pair_record);
            } else {
                Value val;
                if (auto const err{val.decode_from_reader(reader, tag)};
                    IRErrorCode::IRErrorCode_Success != err)
                {
                    return err;
                }
                if (val.is_empty()) {
                    array.push_back(nullptr);
                } else if (val.is_type<value_int_t>()) {
                    array.push_back(val.get<value_int_t>());
                } else if (val.is_type<value_float_t>()) {
                    array.push_back(val.get<value_float_t>());
                } else if (val.is_type<value_bool_t>()) {
                    array.push_back(val.get<value_bool_t>());
                } else if (val.is_type<value_str_t>()) {
                    array.push_back(val.get<value_str_t>());
                } else {
                    return IRErrorCode::IRErrorCode_Corrupted_IR;
                }
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

    if (is_encoded_key_value_pair_tag(encoded_tag)) {
        return deserialize_key_value_pair_record(encoded_tag, reader, schema_tree, obj);
    }

    if (is_array_tag(encoded_tag)) {
        return deserialize_array(encoded_tag, reader, schema_tree, obj);
    }

    return IRErrorCode::IRErrorCode_Decode_Error;
}
}  // namespace ffi::ir_stream
