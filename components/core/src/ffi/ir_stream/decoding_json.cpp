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
               || cProtocol::Payload::SchemaNodeIdShort == tag
               || cProtocol::Payload::KeyValuePairRecordDeliminator == tag;
    }

    [[nodiscard]] auto is_array_tag(encoded_tag_t tag) -> bool {
        return cProtocol::Payload::ArrayBegin == tag;
    }

    [[nodiscard]] auto is_empty_tag(encoded_tag_t tag) -> bool {
        return cProtocol::Payload::EmptyObj == tag || cProtocol::Payload::EmptyArray == tag;
    }

    [[nodiscard]] auto get_empty_array_or_obj(encoded_tag_t tag) -> nlohmann::json {
        if (tag == cProtocol::Payload::EmptyArray) {
            return nlohmann::json::array();
        } else if (tag == cProtocol::Payload::EmptyObj) {
            return nlohmann::json::object();
        }
        return nullptr;
    }

    [[nodiscard]] auto is_new_schema_tree_node_tag(encoded_tag_t tag) -> bool {
        return 0x70 <= tag && tag <= 0x75;
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
        SchemaTreeNodeValueType type{SchemaTreeNodeValueType::Unknown};
        if (false == encoded_tag_to_tree_node_type(encoded_tag, type)) {
            return IRErrorCode::IRErrorCode_Corrupted_IR;
        }

        size_t parent_id{};
        uint16_t decoded_id{};
        if (false == decode_int(reader, decoded_id)) {
            return IRErrorCode::IRErrorCode_Incomplete_IR;
        }
        parent_id = static_cast<size_t>(decoded_id);

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
            return IRErrorCode::IRErrorCode_Mismatching_Tag;
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
    auto insert_key_value_pair(
            size_t decoded_id,
            ValueType const& value,
            SchemaTree const& schema_tree,
            std::vector<std::string_view>& keys_to_root,
            nlohmann::json& obj
    ) -> void {
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
                std::cerr << "Failed to decode key id\n";
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
                    std::cerr << "Failed to deserialize array from obj.\n";
                    return error;
                }
                if (schema_tree.get_node_with_id(id).get_type() != SchemaTreeNodeValueType::Obj) {
                    return IRErrorCode::IRErrorCode_Decode_Error;
                }
                insert_key_value_pair(id, sub_array, schema_tree, keys_to_root, obj);
            } else if (is_empty_tag(tag)) {
                insert_key_value_pair(
                        id,
                        get_empty_array_or_obj(tag),
                        schema_tree,
                        keys_to_root,
                        obj
                );
            } else {
                if (auto const error{deserialize_value(tag, reader, schema_tree, id, value)};
                    IRErrorCode::IRErrorCode_Success != error)
                {
                    std::cerr << "Failed to deserialize value.\n";
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
                        std::cerr << "Decoding error... Unknown tag.\n";
                        return IRErrorCode::IRErrorCode_Decode_Error;
                }
            }
            if (ErrorCode_Success != reader.try_read_numeric_value(tag)) {
                std::cerr << "Incomplete IR decoding\n";
                return IRErrorCode_Incomplete_IR;
            }
        }
        return IRErrorCode_Success;
    }

    [[nodiscard]] auto deserialize_key_value_pairs(
            encoded_tag_t tag,
            ReaderInterface& reader,
            SchemaTree& schema_tree,
            nlohmann::json& obj
    ) -> IRErrorCode {
        std::vector<size_t> ids;
        while (true) {
            size_t key_id;
            if (auto const err{deserialize_key_id(tag, reader, key_id)}; IRErrorCode_Success != err)
            {
                if (IRErrorCode_Mismatching_Tag == err) {
                    break;
                }
                return err;
            }
            ids.push_back(key_id);
            if (ErrorCode_Success != reader.try_read_numeric_value(tag)) {
                return IRErrorCode_Incomplete_IR;
            }
        }

        if (ids.empty()) {
            return IRErrorCode_Corrupted_IR;
        }
        auto const num_keys{ids.size()};
        size_t num_decoded_value{0};
        Value value;
        std::vector<std::string_view> keys_to_root;
        obj = nlohmann::json::object();
        while (true) {
            auto const id{ids[num_decoded_value]};
            auto const type{schema_tree.get_node_with_id(id).get_type()};
            if (is_empty_tag(tag)) {
                if (SchemaTreeNodeValueType::Obj != type) {
                    std::cerr << "Mismatching empty object types.\n";
                    return IRErrorCode_Corrupted_IR;
                }
                insert_key_value_pair(
                        id,
                        get_empty_array_or_obj(tag),
                        schema_tree,
                        keys_to_root,
                        obj
                );
            } else if (is_array_tag(tag)) {
                nlohmann::json sub_array;
                if (schema_tree.get_node_with_id(id).get_type() != SchemaTreeNodeValueType::Obj) {
                    std::cerr << "Mismatching array type.\n";
                    return IRErrorCode::IRErrorCode_Corrupted_IR;
                }
                if (auto const error{deserialize_array(tag, reader, schema_tree, sub_array)};
                    IRErrorCode::IRErrorCode_Success != error)
                {
                    std::cerr << "Failed to deserialize array from obj.\n";
                    return error;
                }
                insert_key_value_pair(id, sub_array, schema_tree, keys_to_root, obj);
            } else {
                if (auto const error{deserialize_value(tag, reader, schema_tree, id, value)};
                    IRErrorCode::IRErrorCode_Success != error)
                {
                    std::cerr << "Failed to deserialize value.\n";
                    return error;
                }
                auto& node{schema_tree.get_node_with_id(id)};
                auto deserialize_int_pair{[&]() {
                    auto const delta{value.get<value_int_t>()};
                    auto const curr_val{node.get_prev_val() + delta};
                    insert_key_value_pair(id, curr_val, schema_tree, keys_to_root, obj);
                    node.set_prev_val(curr_val);
                }};
                switch (node.get_type()) {
                    case SchemaTreeNodeValueType::Int:
                        deserialize_int_pair();
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
                        std::cerr << "Decoding error... Unknown tag.\n";
                        return IRErrorCode::IRErrorCode_Decode_Error;
                }
            }

            ++num_decoded_value;
            if (num_decoded_value == num_keys) {
                break;
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
        std::string array_str;
        if (auto const err{decode_clp_string<four_byte_encoded_variable_t>(reader, array_str)};
            IRErrorCode_Success != err)
        {
            std::cerr << "Failed to decode the clp str.\n";
            return err;
        }
        array = nlohmann::json::parse(array_str);
        if (false == array.is_array()) {
            std::cerr << "The deserialized object is not an array.\n";
            return IRErrorCode_Decode_Error;
        }
        if (ErrorCode_Success != reader.try_read_numeric_value(tag)) {
            std::cerr << "Failed to decode the tag.\n";
            return IRErrorCode_Incomplete_IR;
        }
        if (cProtocol::Payload::ArrayEnd != tag) {
            std::cerr << "The end tag is not Array end.\n";
            return IRErrorCode_Corrupted_IR;
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
        if (false == is_new_schema_tree_node_tag(encoded_tag)) {
            break;
        }
        if (auto const error{schema_tree_growth(encoded_tag, reader, schema_tree)};
            IRErrorCode::IRErrorCode_Success != error)
        {
            std::cerr << "Failed to decode schema tree.\n";
            return error;
        }
    }

    if (is_empty_tag(encoded_tag)) {
        obj = get_empty_array_or_obj(encoded_tag);
        return IRErrorCode_Success;
    }

    if (is_encoded_key_value_pair_tag(encoded_tag)) {
        return deserialize_key_value_pairs(encoded_tag, reader, schema_tree, obj);
    }

    if (is_array_tag(encoded_tag)) {
        std::cerr << "Array as the top level data is not supported.";
        return deserialize_array(encoded_tag, reader, schema_tree, obj);
    }

    return IRErrorCode::IRErrorCode_Decode_Error;
}
}  // namespace ffi::ir_stream
