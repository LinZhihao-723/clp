#include "serialization_methods.hpp"

#include <iostream>
#include <string_view>
#include <span>
#include <vector>

#include "../../../clp/ffi/ir_stream/encoding_methods.hpp"
#include "byteswap.hpp"
#include "protocol_constants.hpp"
#include "utils.hpp"

using std::vector;

namespace clp_s::ffi::ir_stream {
namespace {
/**
 * This tag determines whether integers will be serialized using bytes and shorts.
 */
static constexpr bool cEnableShortIntCompression{false};
static constexpr bool cEnable32IntCompression{false};

/**
 * This class defines a stack node used to traverse a msgpack MAP.
 */
template <typename ElementType>
class SerializationStackNode {
public:
    using View = std::span<ElementType>;

    SerializationStackNode(ElementType* ptr, size_t size, SchemaTreeNode::id_t parent_id)
            : m_view{ptr, size},
              m_parent_id{parent_id} {}

    /**
     * @return true if all the elements have been traversed.
     */
    [[nodiscard]] auto end_reached() const -> bool { return m_view.size() == m_idx; }

    /**
     * Advances the underlying iterator.
     */
    auto advance() -> void { ++m_idx; }

    /**
     * @return a const reference to the next element to be traversed.
     */
    [[nodiscard]] auto get_element() const -> ElementType const& { return m_view[m_idx]; }

    /**
     * @return the parent's schema tree node id.
     */
    [[nodiscard]] auto get_parent_id() const -> SchemaTreeNode::id_t { return m_parent_id; }

private:
    View m_view;
    size_t m_idx{0};
    SchemaTreeNode::id_t m_parent_id;
};

/**
 * Converts a given msgpack value to its corresponded schema tree node type.
 * @param val msgpack value
 * @param type Outputs the corresponded value.
 * @return true if the corresponded schema tree node type is found.
 * @return false if the given object cannot be converted to existing schema tree node types.
 */
[[nodiscard]] auto convert_msgpack_value_to_schema_tree_node_type(
        msgpack::object const& val,
        SchemaTreeNode::Type& type
) -> bool {
    switch (val.type) {
        case msgpack::type::POSITIVE_INTEGER:
        case msgpack::type::NEGATIVE_INTEGER:
            type = SchemaTreeNode::Type::Int;
            break;
        case msgpack::type::FLOAT32:
        case msgpack::type::FLOAT:
            type = SchemaTreeNode::Type::Float;
            break;
        case msgpack::type::STR:
            type = SchemaTreeNode::Type::Str;
            break;
        case msgpack::type::BOOLEAN:
            type = SchemaTreeNode::Type::Bool;
            break;
        case msgpack::type::NIL:
        case msgpack::type::MAP:
            type = SchemaTreeNode::Type::Obj;
            break;
        case msgpack::type::ARRAY:
            type = SchemaTreeNode::Type::Array;
            break;
        default:
            return false;
    }
    return true;
}

/**
 * Serializes the given integer into the IR stream.
 * @tparam integer_t The size of this type must be 1, 2, 4 or 8 bytes.
 * @param value
 * @param buf
 */
template <typename integer_t>
auto serialize_integer_generic(integer_t value, vector<int8_t>& buf) -> void {
    integer_t value_big_endian{};
    static_assert(
            sizeof(integer_t) == 1 || sizeof(integer_t) == 2 || sizeof(integer_t) == 4
            || sizeof(integer_t) == 8
    );
    if constexpr (sizeof(value) == 1) {
        value_big_endian = value;
    } else if constexpr (sizeof(value) == 2) {
        value_big_endian = bswap_16(value);
    } else if constexpr (sizeof(value) == 4) {
        value_big_endian = bswap_32(value);
    } else if constexpr (sizeof(value) == 8) {
        value_big_endian = bswap_64(value);
    }
    auto const* data{reinterpret_cast<int8_t*>(&value_big_endian)};
    buf.insert(buf.end(), data, data + sizeof(value));
}

/**
 * Serializes the given int value (signed) into the IR stream.
 * @param value
 * @param buf
 * @return true on success.
 * @return false if the given value exceeds the largest encoding range.
 */
[[nodiscard]] auto serialize_int(int64_t value, vector<int8_t>& buf) -> bool {
    if (cEnableShortIntCompression && INT8_MIN <= value && value <= INT8_MAX) {
        buf.push_back(cProtocol::Tag::ValueInt8);
        serialize_integer_generic(static_cast<int8_t>(value), buf);
    } else if (cEnableShortIntCompression && INT16_MIN <= value && value <= INT16_MAX) {
        buf.push_back(cProtocol::Tag::ValueInt16);
        serialize_integer_generic(static_cast<int16_t>(value), buf);
    } else if (cEnable32IntCompression && INT32_MIN <= value && value <= INT32_MAX) {
        buf.push_back(cProtocol::Tag::ValueInt32);
        serialize_integer_generic(static_cast<int32_t>(value), buf);
    } else if (INT64_MIN <= value && value <= INT64_MAX) {
        buf.push_back(cProtocol::Tag::ValueInt64);
        serialize_integer_generic(static_cast<int64_t>(value), buf);
    } else {
        // Ideally the control flow will never reach here, but it is added in case we change the
        // input type.
        return false;
    }
    return true;
}

/**
 * Serializes the given double into the IR stream.
 * @param value
 * @param buf
 */
auto serialize_double(double value, vector<int8_t>& buf) -> void {
    buf.push_back(cProtocol::Tag::ValueDouble);
    serialize_integer_generic(std::bit_cast<uint64_t>(value), buf);
}

/**
 * Serializes the given string into the IR stream, using standard string encoding.
 * @param str
 * @param buf
 * @return true on success.
 * @return false on failure if the string length exceeds the encoding range.
 */
[[nodiscard]] auto serialize_str(std::string_view str, vector<int8_t>& buf) -> bool {
    auto const length{str.length()};
    if (length <= UINT8_MAX) {
        buf.push_back(cProtocol::Tag::StandardStrLenByte);
        serialize_integer_generic(static_cast<uint8_t>(length), buf);
    } else if (length <= UINT16_MAX) {
        buf.push_back(cProtocol::Tag::StandardStrLenShort);
        serialize_integer_generic(static_cast<uint16_t>(length), buf);
    } else if (length <= UINT32_MAX) {
        buf.push_back(cProtocol::Tag::StandardStrLenInt);
        serialize_integer_generic(static_cast<uint32_t>(length), buf);
    } else {
        return false;
    }
    buf.insert(buf.cend(), str.cbegin(), str.cend());
    return true;
}

/**
 * Serializes the given string into the IR stream, using CLP string encoding.
 * @param str
 * @param buf
 * @return true on success, false on failure.
 */
[[nodiscard]] auto serialize_clp_str(std::string_view str, vector<int8_t>& buf) -> bool {
    constexpr bool cUseFourByteEncoding{false};
    if constexpr (cUseFourByteEncoding) {
        buf.push_back(cProtocol::Tag::ValueStrCLPFourByte);
        std::string logtype;
        return clp::ffi::ir_stream::four_byte_encoding::serialize_message(str, logtype, buf);
    } else {
        buf.push_back(cProtocol::Tag::ValueStrCLPEightByte);
        std::string logtype;
        return clp::ffi::ir_stream::eight_byte_encoding::serialize_message(str, logtype, buf);
    }
}

/**
 * Serializes the given boolean value into the IR stream.
 * @param value
 * @param buf
 */
auto serialize_bool(bool value, vector<int8_t>& buf) -> void {
    auto const tag{value ? cProtocol::Tag::ValueTrue : cProtocol::Tag::ValueFalse};
    buf.push_back(tag);
}

/**
 * Serializes the null value into the IR stream.
 * @param buf
 */
auto serialize_null(vector<int8_t>& buf) -> void {
    buf.push_back(cProtocol::Tag::ValueNull);
}

/**
 * Serializes an empty object into the IR stream.
 * @param buf
 */
auto serialize_empty_obj(vector<int8_t>& buf) -> void {
    buf.push_back(cProtocol::Tag::ValueEmpty);
}

/**
 * Serializes a new schema tree node into the IR stream.
 * @param locator The location information in the tree.
 * @param buf
 * @return true on success, false on failure.
 */
[[nodiscard]] auto
serialize_new_schema_tree_node(SchemaTree::TreeNodeLocator const& locator, vector<int8_t>& buf)
        -> bool {
    switch (locator.get_type()) {
        case SchemaTreeNode::Type::Int:
            buf.push_back(cProtocol::Tag::SchemaNodeInt);
            break;
        case SchemaTreeNode::Type::Float:
            buf.push_back(cProtocol::Tag::SchemaNodeFloat);
            break;
        case SchemaTreeNode::Type::Bool:
            buf.push_back(cProtocol::Tag::SchemaNodeBool);
            break;
        case SchemaTreeNode::Type::Str:
            buf.push_back(cProtocol::Tag::SchemaNodeStr);
            break;
        case SchemaTreeNode::Type::Obj:
            buf.push_back(cProtocol::Tag::SchemaNodeObj);
            break;
        case SchemaTreeNode::Type::Array:
            buf.push_back(cProtocol::Tag::SchemaNodeArray);
            break;
        default:
            // Unknown type
            return false;
    }
    auto const parent_id{locator.get_parent_id()};
    if (UINT8_MAX >= parent_id) {
        buf.push_back(cProtocol::Tag::SchemaNodeParentIdByte);
        serialize_integer_generic(static_cast<uint8_t>(parent_id), buf);
    } else if (UINT16_MAX >= parent_id) {
        buf.push_back(cProtocol::Tag::SchemaNodeParentIdShort);
        serialize_integer_generic(static_cast<uint16_t>(parent_id), buf);
    } else {
        return false;
    }
    return serialize_str(locator.get_key_name(), buf);
}

/**
 * Serializes an array as a JSON str into the IR stream.
 * @param array
 * @param buf
 * @return true one success, false on failure.
 */
[[nodiscard]] auto serialize_array_as_json_str(msgpack::object const& array, vector<int8_t>& buf)
        -> bool {
    std::string json_str;
    if (false == append_msgpack_array_to_json_str(array, json_str)) {
        return false;
    }
    return serialize_clp_str(json_str, buf);
}

/**
 * Serializes the given key id (SchemaTreeNodeId) into the IR stream.
 * @param id
 * @param buf
 * @return true one success, false on failure.
 */
[[nodiscard]] auto serialize_key_id(SchemaTreeNode::id_t id, vector<int8_t>& buf) -> bool {
    if (UINT8_MAX >= id) {
        buf.push_back(cProtocol::Tag::KeyIdByte);
        serialize_integer_generic(static_cast<uint8_t>(id), buf);
    } else if (UINT16_MAX >= id) {
        buf.push_back(cProtocol::Tag::KeyIdShort);
        serialize_integer_generic(static_cast<uint16_t>(id), buf);
    } else {
        return false;
    }
    return true;
}

/**
 * Serializes a string value into the IR stream, using either basic string encoding or CLP string
 * encoding according to the heuristic.
 * @param str
 * @param buf
 * @return true one success, false on failure.
 */
[[nodiscard]] auto serialize_str_val(std::string_view str, vector<int8_t>& buf) -> bool {
    if (std::string_view::npos == str.find(' ')) {
        return serialize_str(str, buf);
    }
    return serialize_clp_str(str, buf);
}

/**
 * Serializes a value into the IR stream.
 * @param val
 * @param type
 * @param buf
 * @return true one success, false on failure.
 */
[[nodiscard]] auto
serialize_value(msgpack::object const& val, SchemaTreeNode::Type type, vector<int8_t>& buf)
        -> bool {
    switch (type) {
        case SchemaTreeNode::Type::Int:
            if (msgpack::type::POSITIVE_INTEGER == val.type
                && static_cast<uint64_t>(INT64_MAX) < val.as<uint64_t>())
            {
                return false;
            }
            if (false == serialize_int(val.as<int64_t>(), buf)) {
                return false;
            }
            break;
        case SchemaTreeNode::Type::Float:
            serialize_double(val.as<double>(), buf);
            break;
        case SchemaTreeNode::Type::Bool:
            serialize_bool(val.as<bool>(), buf);
            break;
        case SchemaTreeNode::Type::Str:
            if (false == serialize_str_val(val.as<std::string_view>(), buf)) {
                return false;
            }
            break;
        case SchemaTreeNode::Type::Array:
            if (false == serialize_array_as_json_str(val, buf)) {
                return false;
            }
            break;
        case SchemaTreeNode::Type::Obj:
            if (msgpack::type::NIL != val.type) {
                return false;
            }
            serialize_null(buf);
            break;
        default:
            // Unknown type
            return false;
    }
    return true;
}
}  // namespace

auto serialize_key_value_pair_record(
        msgpack::object const& record,
        SerializationBuffer& serialization_buf
) -> bool {
    if (msgpack::type::MAP != record.type) {
        return false;
    }
    auto const& map{record.via.map};
    if (0 == map.size) {
        serialize_empty_obj(serialization_buf.m_ir_buf);
        return true;
    }

    vector<SerializationStackNode<std::remove_pointer_t<decltype(map.ptr)>>> working_stack;
    working_stack.emplace_back(map.ptr, static_cast<size_t>(map.size), SchemaTree::cRootId);
    serialization_buf.m_schema_tree.take_snapshot();

    auto& key_buf{serialization_buf.m_key_group_buf};
    auto& val_buf{serialization_buf.m_value_group_buf};
    auto& node_buf{serialization_buf.m_schema_tree_node_buf};
    key_buf.clear();
    val_buf.clear();
    node_buf.clear();

    bool failure{false};
    while (false == working_stack.empty()) {
        auto& curr{working_stack.back()};
        if (curr.end_reached()) {
            working_stack.pop_back();
            continue;
        }
        auto const& [key, val]{curr.get_element()};
        curr.advance();
        SchemaTreeNode::Type schema_tree_node_type{};
        if (false == convert_msgpack_value_to_schema_tree_node_type(val, schema_tree_node_type)) {
            std::cerr << "1\n";
            failure = true;
            break;
        }
        SchemaTree::TreeNodeLocator locator{
                curr.get_parent_id(),
                key.as<std::string_view>(),
                schema_tree_node_type};
        SchemaTreeNode::id_t curr_id{};
        if (false == serialization_buf.m_schema_tree.has_node(locator, curr_id)) {
            curr_id = serialization_buf.m_schema_tree.insert_node(locator);
            if (false == serialize_new_schema_tree_node(locator, node_buf)) {
                std::cerr << "2\n";
                failure = true;
                break;
            }
        }
        if (SchemaTreeNode::Type::Obj == schema_tree_node_type && msgpack::type::MAP == val.type) {
            auto const& inner_map{val.via.map};
            auto const inner_map_size{static_cast<size_t>(inner_map.size)};
            if (0 == inner_map_size) {
                if (false == serialize_key_id(curr_id, key_buf)) {
                    std::cerr << "3\n";
                    failure = true;
                    break;
                }
                serialize_empty_obj(val_buf);
            } else {
                working_stack.emplace_back(inner_map.ptr, inner_map_size, curr_id);
            }
        } else {
            if (false == serialize_key_id(curr_id, key_buf)) {
                std::cerr << "4\n";
                failure = true;
                break;
            }
            if (false == serialize_value(val, schema_tree_node_type, val_buf)) {
                std::cerr << "5\n";
                failure = true;
                break;
            }
        }
    }

    if (failure) {
        serialization_buf.m_schema_tree.revert();
        return false;
    }

    auto& ir_buf{serialization_buf.m_ir_buf};
    ir_buf.insert(ir_buf.cend(), node_buf.cbegin(), node_buf.cend());
    ir_buf.insert(ir_buf.cend(), key_buf.cbegin(), key_buf.cend());
    ir_buf.insert(ir_buf.cend(), val_buf.cbegin(), val_buf.cend());

    return true;
}

auto serialize_end_of_stream(SerializationBuffer& serialization_buf) -> void {
    serialization_buf.m_ir_buf.push_back(cProtocol::EndOfStream);
}
}  // namespace clp_s::ffi::ir_stream
