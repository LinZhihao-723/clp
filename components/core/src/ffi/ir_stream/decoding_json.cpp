#include "decoding_json.hpp"

#include <iostream>

#include "protocol_constants.hpp"

namespace ffi::ir_stream {
namespace {
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

    return IRErrorCode_Success;
}
}  // namespace ffi::ir_stream
