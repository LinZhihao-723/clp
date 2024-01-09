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

    // Now, only decode key-value pairs.
    Value value;
    size_t id{};
    std::vector<std::pair<size_t, Value>> key_value_pairs;
    while (true) {
        if (cProtocol::Payload::KeyValuePairRecordDeliminator == encoded_tag) {
            break;
        }
        if (auto const error{
                    deserialize_key_value_pair(encoded_tag, reader, schema_tree, id, value)};
            IRErrorCode::IRErrorCode_Success != error)
        {
            return error;
        }
        key_value_pairs.emplace_back(id, value);
        if (ErrorCode_Success != reader.try_read_numeric_value(encoded_tag)) {
            return IRErrorCode_Incomplete_IR;
        }
    }

    std::cerr << "Deserialized Key Value Pair:\n";
    std::vector<std::string_view> key_chain_to_root;
    for (auto const& [decoded_id, decoded_value] : key_value_pairs) {
        key_chain_to_root.clear();
        size_t curr_id{decoded_id};
        while (SchemaTree::cRootId != curr_id) {
            auto const& node{schema_tree.get_node_with_id(curr_id)};
            key_chain_to_root.push_back(node.get_key_name());
            curr_id = node.get_parent_id();
        }
        auto const num_keys{key_chain_to_root.size()};
        for (int i{static_cast<int>(num_keys - 1)}; 0 <= i; --i) {
            std::cerr << key_chain_to_root[i] << ":";
        }
        std::cerr << " " << decoded_value.dump() << "\n";
    }

    return IRErrorCode_Success;
}
}  // namespace ffi::ir_stream
