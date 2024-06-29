#include "deserialization_methods.hpp"

#include <iostream>
#include <unordered_map>

#include <json/single_include/nlohmann/json.hpp>

#include "../../../clp/ErrorCode.hpp"
#include "../../../clp/ffi/ir_stream/decoding_methods.hpp"
#include "../../Utils.hpp"
#include "byteswap.hpp"
#include "ClpString.hpp"
#include "protocol_constants.hpp"

using clp::ReaderInterface;
using clp_s::ffi::ir_stream::cProtocol::encoded_tag_t;
using std::vector;

namespace clp_s::ffi::ir_stream {
namespace {
/**
 * @return true if the tag indicates the end of the stream.
 */
[[nodiscard]] auto is_end_of_stream(encoded_tag_t tag) -> bool {
    return cProtocol::EndOfStream == tag;
}

/**
 * @return true if the tag represents a new schema tree node.
 */
[[nodiscard]] auto is_new_schema_tree_node(encoded_tag_t tag) -> bool {
    return 0x71 <= tag && tag <= 0x76;
}

/**
 * Reads the next byte as the tag from the reader.
 * @param reader
 * @param tag Outputs the tag
 * @return IRErrorCode::Success on success.
 * @return IRErrorCode::EndOfStream if next read tag represents the end of the stream.
 * @return IRErrorCode::IncompleteStream is it fails to read the next tag.
 */
[[nodiscard]] auto read_next_tag(ReaderInterface& reader, encoded_tag_t& tag) -> IRErrorCode {
    if (clp::ErrorCode_Success != reader.try_read_numeric_value(tag)) {
        return IRErrorCode::IncompleteStream;
    }
    if (cProtocol::EndOfStream == tag) {
        return IRErrorCode::EndOfStream;
    }
    return IRErrorCode::Success;
}

/**
 * Deserializes an integer from the given reader.
 * @tparam integer_t Type of the integer to deserialize.
 * @param reader
 * @param value Returns the deserialized integer.
 * @return true on success, false if the reader doesn't contain enough data to deserialize.
 */
template <typename integer_t>
[[nodiscard]] auto deserialize_int(ReaderInterface& reader, integer_t& value) -> bool {
    integer_t value_little_endian;
    if (clp::ErrorCode_Success != reader.try_read_numeric_value(value_little_endian)) {
        return false;
    }

    constexpr auto read_size{sizeof(integer_t)};
    static_assert(read_size == 1 || read_size == 2 || read_size == 4 || read_size == 8);
    if constexpr (read_size == 1) {
        value = value_little_endian;
    } else if constexpr (read_size == 2) {
        value = bswap_16(value_little_endian);
    } else if constexpr (read_size == 4) {
        value = bswap_32(value_little_endian);
    } else if constexpr (read_size == 8) {
        value = bswap_64(value_little_endian);
    }
    return true;
}

/**
 * Deserializes a string from the reader whose length is specified by the given tag.
 * @param reader
 * @param tag The tag that specifies the string length.
 * @param str Outputs the deserialized string.
 * @param IRErrorCode::Success on success.
 * @return related error code otherwise.
 */
[[nodiscard]] auto deserialize_str(ReaderInterface& reader, encoded_tag_t tag, std::string& str)
        -> IRErrorCode {
    size_t str_length{};
    if (cProtocol::Tag::StandardStrLenByte == tag) {
        uint8_t length{};
        if (false == deserialize_int(reader, length)) {
            return IRErrorCode::IncompleteStream;
        }
        str_length = static_cast<size_t>(length);
    } else if (cProtocol::Tag::StandardStrLenShort == tag) {
        uint16_t length{};
        if (false == deserialize_int(reader, length)) {
            return IRErrorCode::IncompleteStream;
        }
        str_length = static_cast<size_t>(length);
    } else if (cProtocol::Tag::StandardStrLenInt == tag) {
        uint32_t length{};
        if (false == deserialize_int(reader, length)) {
            return IRErrorCode::IncompleteStream;
        }
        str_length = static_cast<size_t>(length);
    } else {
        std::cerr << "Unknown tag str\n";
        return IRErrorCode::UnknownTag;
    }
    if (clp::ErrorCode_Success != reader.try_read_string(str_length, str)) {
        return IRErrorCode::IncompleteStream;
    }
    return IRErrorCode::Success;
}

/**
 * Converts a tag to its corresponded schema tree node type.
 * @param tag
 * @return the schema tree node type represented by the tag.
 * @throw DeserializingException is the tag does not indicate any schema tree node types.
 */
[[nodiscard]] auto convert_tag_to_schema_tree_node_type(encoded_tag_t tag) -> SchemaTreeNode::Type {
    switch (tag) {
        case cProtocol::Tag::SchemaNodeInt:
            return SchemaTreeNode::Type::Int;
        case cProtocol::Tag::SchemaNodeFloat:
            return SchemaTreeNode::Type::Float;
        case cProtocol::Tag::SchemaNodeBool:
            return SchemaTreeNode::Type::Bool;
        case cProtocol::Tag::SchemaNodeStr:
            return SchemaTreeNode::Type::Str;
        case cProtocol::Tag::SchemaNodeArray:
            return SchemaTreeNode::Type::Array;
        case cProtocol::Tag::SchemaNodeObj:
            return SchemaTreeNode::Type::Obj;
        default:
            throw DeserializingException(
                    ErrorCodeFailure,
                    __FILE__,
                    __LINE__,
                    "Unknown schema tree node type."
            );
    }
}

/**
 * Deserializes the parent id of a schema tree node from the reader.
 * @param reader
 * @param tag
 * @param parent_id Outputs the deserialized parent id.
 * @return IRErrorCode::Success on success.
 * @return related error code otherwise.
 */
[[nodiscard]] auto
deserialize_parent_id(ReaderInterface& reader, encoded_tag_t tag, SchemaTreeNode::id_t& parent_id)
        -> IRErrorCode {
    if (cProtocol::Tag::SchemaNodeParentIdByte == tag) {
        uint8_t deserialized_id{};
        if (false == deserialize_int(reader, deserialized_id)) {
            return IRErrorCode::IncompleteStream;
        }
        parent_id = static_cast<SchemaTreeNode::id_t>(deserialized_id);
    } else if (cProtocol::Tag::SchemaNodeParentIdShort == tag) {
        uint16_t deserialized_id{};
        if (false == deserialize_int(reader, deserialized_id)) {
            return IRErrorCode::IncompleteStream;
        }
        parent_id = static_cast<SchemaTreeNode::id_t>(deserialized_id);
    } else {
        std::cerr << "Unknown tag parent id\n";
        return IRErrorCode::UnknownTag;
    }
    return IRErrorCode::Success;
}

/**
 * Deserializes a new schema tree node from the reader and adds it to the schema tree.
 * @param reader
 * @param tag
 * @param schema_tree
 * @return IRErrorCode::Success on success.
 * @return related error code otherwise.
 */
[[nodiscard]] auto deserialize_new_node_to_schema_tree(
        ReaderInterface& reader,
        encoded_tag_t tag,
        SchemaTree& schema_tree
) -> IRErrorCode {
    auto const type{convert_tag_to_schema_tree_node_type(tag)};

    if (auto const err{read_next_tag(reader, tag)}; IRErrorCode::Success != err) {
        return err;
    }
    SchemaTreeNode::id_t pid{};
    if (auto const err{deserialize_parent_id(reader, tag, pid)}; IRErrorCode::Success != err) {
        return err;
    }

    if (auto const err{read_next_tag(reader, tag)}; IRErrorCode::Success != err) {
        return err;
    }
    std::string key_name;
    if (auto const err{deserialize_str(reader, tag, key_name)}; IRErrorCode::Success != err) {
        return err;
    }

    SchemaTree::TreeNodeLocator locator{pid, key_name, type};
    SchemaTreeNode::id_t id{};
    if (schema_tree.has_node(locator, id)) {
        // The node has already been inserted to the stream.
        return IRErrorCode::CorruptedStream;
    }

    schema_tree.insert_node(locator);
    return IRErrorCode::Success;
}

/**
 * @return true if the tag represents an empty value.
 */
[[nodiscard]] auto is_empty_value(encoded_tag_t tag) -> bool {
    return cProtocol::Tag::ValueEmpty == tag;
}

/**
 * Deserializes an integer value from the reader and appends it to the end of the deserialized
 * values.
 * @tparam integer_t
 * @param reader
 * @param values
 * @return IRErrorCode::Success on success.
 * @return related error code otherwise.
 */
template <typename integer_t>
[[nodiscard]] auto
deserialize_and_append_integer_value(ReaderInterface& reader, vector<std::optional<Value>>& values)
        -> IRErrorCode {
    integer_t int_val{};
    if (false == deserialize_int(reader, int_val)) {
        return IRErrorCode::IncompleteStream;
    }
    values.emplace_back(static_cast<value_int_t>(int_val));
    return IRErrorCode::Success;
}

/**
 * Deserializes a float value from the reader and appends it to the end of the deserialized
 * values.
 * @param reader
 * @param values
 * @return IRErrorCode::Success on success.
 * @return related error code otherwise.
 */
[[nodiscard]] auto
deserialize_and_append_float_value(ReaderInterface& reader, vector<std::optional<Value>>& values)
        -> IRErrorCode {
    uint64_t int_val{};
    if (false == deserialize_int(reader, int_val)) {
        return IRErrorCode::IncompleteStream;
    }
    values.emplace_back(bit_cast<value_float_t>(int_val));
    return IRErrorCode::Success;
}

/**
 * Deserializes a string value from the reader and appends it to the end of the deserialized
 * values.
 * @param reader
 * @param tag
 * @param values
 * @return IRErrorCode::Success on success.
 * @return related error code otherwise.
 */
[[nodiscard]] auto deserialize_and_append_str_value(
        ReaderInterface& reader,
        encoded_tag_t tag,
        vector<std::optional<Value>>& values
) -> IRErrorCode {
    std::string str;
    if (auto const err{deserialize_str(reader, tag, str)}; IRErrorCode::Success != err) {
        return err;
    }
    values.emplace_back(std::move(str));
    return IRErrorCode::Success;
}

/**
 * Deserializes a CLP string from the reader and appends it to the end of the deserialized values.
 * @tparam encoded_variable_t
 * @param reader
 * @param values
 * @return IRErrorCode::Success on success.
 * @return related error code otherwise.
 */
template <typename encoded_variable_t>
[[nodiscard]] auto
deserialize_and_append_clp_str_value(ReaderInterface& reader, vector<std::optional<Value>>& values)
        -> IRErrorCode {
    ClpString<encoded_variable_t> clp_str;
    clp::ffi::ir_stream::IRErrorCode err{};
    if constexpr (std::is_same_v<encoded_variable_t, four_byte_encoded_variable_t>) {
        err = clp::ffi::ir_stream::four_byte_encoding::deserialize_clp_str(
                reader,
                clp_str.get_logtype(),
                clp_str.get_encoded_vars(),
                clp_str.get_dict_vars()
        );
    } else {
        err = clp::ffi::ir_stream::eight_byte_encoding::deserialize_clp_str(
                reader,
                clp_str.get_logtype(),
                clp_str.get_encoded_vars(),
                clp_str.get_dict_vars()
        );
    }
    if (clp::ffi::ir_stream::IRErrorCode_Success != err) {
        if (clp::ffi::ir_stream::IRErrorCode_Incomplete_IR == err) {
            return IRErrorCode::IncompleteStream;
        }
        if (clp::ffi::ir_stream::IRErrorCode_Eof == err) {
            return IRErrorCode::EndOfStream;
        }
        return IRErrorCode::DecodeError;
    }
    values.emplace_back(std::move(clp_str));
    return IRErrorCode::Success;
}

/**
 * Deserializes the next value from the reader and appends it to the end of the deserialized values.
 * @param reader
 * @param tag
 * @param values
 * @return IRErrorCode::Success on success.
 * @return related error code otherwise.
 */
[[nodiscard]] auto deserialize_and_append_value(
        ReaderInterface& reader,
        encoded_tag_t tag,
        vector<std::optional<Value>>& values
) -> IRErrorCode {
    IRErrorCode err{IRErrorCode::Success};
    switch (tag) {
        case cProtocol::Tag::ValueInt8:
            err = deserialize_and_append_integer_value<int8_t>(reader, values);
            break;
        case cProtocol::Tag::ValueInt16:
            err = deserialize_and_append_integer_value<int16_t>(reader, values);
            break;
        case cProtocol::Tag::ValueInt32:
            err = deserialize_and_append_integer_value<int32_t>(reader, values);
            break;
        case cProtocol::Tag::ValueInt64:
            err = deserialize_and_append_integer_value<int64_t>(reader, values);
            break;
        case cProtocol::Tag::ValueDouble:
            err = deserialize_and_append_float_value(reader, values);
            break;
        case cProtocol::Tag::ValueTrue:
            values.emplace_back(true);
            break;
        case cProtocol::Tag::ValueFalse:
            values.emplace_back(false);
            break;
        case cProtocol::Tag::StandardStrLenByte:
        case cProtocol::Tag::StandardStrLenShort:
        case cProtocol::Tag::StandardStrLenInt:
            err = deserialize_and_append_str_value(reader, tag, values);
            break;
        case cProtocol::Tag::ValueStrCLPFourByte:
            err = deserialize_and_append_clp_str_value<four_byte_encoded_variable_t>(
                    reader,
                    values
            );
            break;
        case cProtocol::Tag::ValueStrCLPEightByte:
            err = deserialize_and_append_clp_str_value<eight_byte_encoded_variable_t>(
                    reader,
                    values
            );
            break;
        case cProtocol::Tag::ValueEmpty:
            values.emplace_back(std::nullopt);
            break;
        case cProtocol::Tag::ValueNull:
            values.emplace_back(Value{});
            break;
        default:
            std::cerr << "Unknown tag value: " << static_cast<int>(tag) << "\n";
            err = IRErrorCode::UnknownTag;
            break;
    }
    return err;
}

/**
 * Checks whether the value type matches the schema tree node type.
 * @param type
 * @param optional_val
 * @return true if the type matches, false otherwise.
 */
[[nodiscard]] auto
check_value_type(SchemaTreeNode::Type type, std::optional<Value> const& optional_val) -> bool {
    if (false == optional_val.has_value()) {
        return SchemaTreeNode::Type::Obj == type;
    }
    auto const& val{optional_val.value()};
    switch (type) {
        case SchemaTreeNode::Type::Int:
            return val.is_type<value_int_t>();
        case SchemaTreeNode::Type::Float:
            return val.is_type<value_float_t>();
        case SchemaTreeNode::Type::Bool:
            return val.is_type<value_bool_t>();
        case SchemaTreeNode::Type::Str:
            return val.is_type<value_str_t>() || val.is_clp_str();
        case SchemaTreeNode::Type::Array:
            return val.is_clp_str();
        case SchemaTreeNode::Type::Obj:
            return val.is_null();
        default:
            return false;
    }
}

/**
 * Appends a leaf node to the JSON string.
 * @param node
 * @param opt_val
 * @param json_str Outputs the appended JSON string.
 * @param add_leading_comma Whether to add `,` before appending the leaf node.
 */
auto append_leaf_node_to_json_str(
        SchemaTreeNode const& node,
        std::optional<Value> const& opt_val,
        bool add_leading_comma,
        std::string& json_str
) -> void {
    if (add_leading_comma) {
        json_str.push_back(',');
    }
    json_str.push_back('\"');
    json_str += node.get_key_name();
    json_str.push_back('\"');
    json_str.push_back(':');
    if (false == opt_val.has_value()) {
        json_str += "{}";
        return;
    }
    auto const& val{opt_val.value()};
    if (SchemaTreeNode::Type::Str == node.get_type()) {
        json_str += nlohmann::json(val.dump()).dump();
    } else {
        json_str += val.dump();
    }
}
}  // namespace

auto deserialize_next_key_value_pair_record(
        ReaderInterface& reader,
        SchemaTree& schema_tree,
        vector<SchemaTreeNode::id_t>& schema,
        vector<std::optional<Value>>& values
) -> IRErrorCode {
    encoded_tag_t tag{cProtocol::EndOfStream};
    schema.clear();
    values.clear();

    // Deserialize new schema tree nodes
    while (true) {
        if (auto const err{read_next_tag(reader, tag)}; IRErrorCode::Success != err) {
            return err;
        }
        if (false == is_new_schema_tree_node(tag)) {
            break;
        }
        if (auto const err{deserialize_new_node_to_schema_tree(reader, tag, schema_tree)};
            IRErrorCode::Success != err)
        {
            return err;
        }
    }

    // Deserialize schema
    while (true) {
        if (cProtocol::Tag::KeyIdByte == tag) {
            uint8_t id{};
            if (false == deserialize_int(reader, id)) {
                return IRErrorCode::IncompleteStream;
            }
            schema.push_back(static_cast<SchemaTreeNode::id_t>(id));
        } else if (cProtocol::Tag::KeyIdShort == tag) {
            uint16_t id{};
            if (false == deserialize_int(reader, id)) {
                return IRErrorCode::IncompleteStream;
            }
            schema.push_back(static_cast<SchemaTreeNode::id_t>(id));
        } else {
            break;
        }
        if (auto const err{read_next_tag(reader, tag)}; IRErrorCode::Success != err) {
            return err;
        }
    }

    auto const num_leaves{schema.size()};
    if (0 == num_leaves) {
        if (is_empty_value(tag)) {
            // The record is empty
            return IRErrorCode::Success;
        }
        return IRErrorCode::CorruptedStream;
    }

    // Deserialize values
    size_t idx{0};
    while (true) {
        if (auto const err{deserialize_and_append_value(reader, tag, values)};
            IRErrorCode::Success != err)
        {
            return err;
        }
        if (false
            == check_value_type(
                    schema_tree.get_node_with_id(schema[idx]).get_type(),
                    values.back()
            ))
        {
            return IRErrorCode::CorruptedStream;
        }
        if (values.size() == num_leaves) {
            break;
        }
        if (auto const err{read_next_tag(reader, tag)}; IRErrorCode::Success != err) {
            return err;
        }
        ++idx;
    }

    return IRErrorCode::Success;
}

[[nodiscard]] auto deserialize_record_as_json_str(
        SchemaTree const& schema_tree,
        std::vector<SchemaTreeNode::id_t> const& schema,
        std::vector<std::optional<Value>> const& values,
        std::string& json_str
) -> bool {
    struct StackNode {
        StackNode(SchemaTreeNode const& node)
                : node_ref{node},
                  num_children{node.get_children_ids().size()} {};
        SchemaTreeNode const& node_ref;
        size_t num_children;
        size_t child_idx{0};
        size_t num_serialized_child{0};
    };

    json_str.clear();
    if (schema.size() != values.size()) {
        return false;
    }
    if (values.empty()) {
        json_str = "{}";
        return true;
    }

    auto node_to_traverse{std::vector<bool>(schema_tree.get_size(), false)};
    std::unordered_map<SchemaTreeNode::id_t, size_t> id_to_value_map;
    size_t idx{0};

    for (auto id : schema) {
        id_to_value_map.emplace(id, idx);
        ++idx;
        auto id_to_process{id};
        while (SchemaTree::cRootId != id_to_process && false == node_to_traverse[id_to_process]) {
            node_to_traverse[id_to_process] = true;
            id_to_process = schema_tree.get_node_with_id(id_to_process).get_parent_id();
        }
    }

    vector<StackNode> working_stack;
    working_stack.emplace_back(schema_tree.get_node_with_id(SchemaTree::cRootId));
    json_str.push_back('{');
    while (false == working_stack.empty()) {
        auto& top{working_stack.back()};
        if (top.num_children == top.child_idx) {
            json_str.push_back('}');
            working_stack.pop_back();
            continue;
        }
        auto const child_id{top.node_ref.get_children_ids()[top.child_idx]};
        ++top.child_idx;
        if (false == node_to_traverse[child_id]) {
            continue;
        }
        auto const add_comma{0 != top.num_serialized_child};
        ++top.num_serialized_child;
        auto const id_to_value{id_to_value_map.find(child_id)};
        auto const& child{schema_tree.get_node_with_id(child_id)};
        if (id_to_value_map.cend() != id_to_value) {
            // Leaf node reached
            auto const value_idx{id_to_value->second};
            append_leaf_node_to_json_str(child, values[value_idx], add_comma, json_str);
            continue;
        }

        if (add_comma) {
            json_str.push_back(',');
        }
        json_str.push_back('\"');
        json_str += child.get_key_name();
        json_str.push_back('\"');
        json_str.push_back(':');
        json_str.push_back('{');
        working_stack.emplace_back(child);
    }

    return true;
}
}  // namespace clp_s::ffi::ir_stream
