#include "Values.hpp"

#include "decoding_methods.hpp"
#include "encoding_methods.hpp"
#include "protocol_constants.hpp"

namespace ffi::ir_stream {
namespace {
    /**
     * Encodes the given integer value into IR.
     * @param value
     * @param ir_buf
     * @return true on success.
     * @return false on failure.
     */
    auto encode_value_int(value_int_t value, std::vector<int8_t>& ir_buf) -> bool {
        bool success{true};
        if (INT32_MIN <= value && value <= INT32_MAX) {
            ir_buf.push_back(cProtocol::Payload::ValueInt32);
            encode_int(static_cast<int32_t>(value), ir_buf);
        } else if (INT64_MIN <= value && value <= INT64_MAX) {
            ir_buf.push_back(cProtocol::Payload::ValueInt64);
            encode_int(static_cast<int64_t>(value), ir_buf);
        } else {
            success = false;
        }
        return success;
    }

    /**
     * Decodes an integer value into the reader.
     * @param reader
     * @param tag
     * @param value Returns the decoded value.
     * @return IRErrorCode_Success on success
     * @return IRErrorCode_Corrupted_IR if reader contains invalid IR
     * @return IRErrorCode_Incomplete_IR if reader doesn't contain enough data
     */
    auto decode_value_int(ReaderInterface& reader, int8_t tag, value_t& value) -> IRErrorCode {
        if (cProtocol::Payload::ValueInt32 == tag) {
            int32_t int_value{};
            if (false == decode_int(reader, int_value)) {
                return IRErrorCode_Incomplete_IR;
            }
            value = static_cast<value_int_t>(int_value);
        } else if (cProtocol::Payload::ValueInt64 == tag) {
            int64_t int_value{};
            if (false == decode_int(reader, int_value)) {
                return IRErrorCode_Incomplete_IR;
            }
            value = static_cast<value_int_t>(int_value);
        } else {
            return IRErrorCode_Corrupted_IR;
        }
        return IRErrorCode_Success;
    }

    /**
     * Encodes the given float value into IR.
     * @param value
     * @param ir_buf
     * @return true on success.
     * @return false on failure.
     */
    auto encode_value_float(value_float_t value, std::vector<int8_t>& ir_buf) -> bool {
        static_assert(std::is_same_v<value_float_t, double>, "Currently only double is supported.");
        ir_buf.push_back(cProtocol::Payload::ValueDouble);
        encode_floating_number(value, ir_buf);
        return true;
    }

    /**
     * Decodes a float value into the reader.
     * @param reader
     * @param tag
     * @param value Returns the decoded value.
     * @return IRErrorCode_Success on success
     * @return IRErrorCode_Corrupted_IR if reader contains invalid IR
     * @return IRErrorCode_Incomplete_IR if reader doesn't contain enough data
     */
    auto decode_value_float(ReaderInterface& reader, int8_t tag, value_t& value) -> IRErrorCode {
        if (cProtocol::Payload::ValueDouble == tag) {
            double float_value{};
            if (false == decode_floating_number(reader, float_value)) {
                return IRErrorCode_Incomplete_IR;
            }
            value = float_value;
        } else {
            return IRErrorCode_Corrupted_IR;
        }
        return IRErrorCode_Success;
    }

    /**
     * Encodes the given boolean value into IR.
     * @param value
     * @param ir_buf
     * @return true on success.
     * @return false on failure.
     */
    auto encode_value_bool(value_bool_t value, std::vector<int8_t>& ir_buf) -> bool {
        if (value) {
            ir_buf.push_back(cProtocol::Payload::ValueTrue);
        } else {
            ir_buf.push_back(cProtocol::Payload::ValueFalse);
        }
        return true;
    }

    /**
     * Encodes the given string value as a normal string.
     * @param value
     * @param ir_buf
     * @return true on success.
     * @return false on failure.
     */
    auto encode_normal_str(std::string_view value, std::vector<int8_t>& ir_buf) -> bool {
        auto const length{value.length()};
        if (length <= UINT8_MAX) {
            ir_buf.push_back(cProtocol::Payload::ValueStrLenUByte);
            auto const length_in_byte{static_cast<uint8_t>(length)};
            ir_buf.push_back(static_cast<int8_t>(length_in_byte));
        } else if (length <= UINT16_MAX) {
            ir_buf.push_back(cProtocol::Payload::ValueStrLenUShort);
            encode_int(static_cast<uint16_t>(length), ir_buf);
        } else if (length <= UINT32_MAX) {
            ir_buf.push_back(cProtocol::Payload::ValueStrLenUInt);
            encode_int(static_cast<uint32_t>(length), ir_buf);
        } else {
            return false;
        }
        ir_buf.insert(ir_buf.cend(), value.cbegin(), value.cend());
        return true;
    }

    /**
     * Encodes the given string value into IR.
     * @param value
     * @param ir_buf
     * @return true on success.
     * @return false on failure.
     */
    auto encode_value_str(std::string_view value, std::vector<int8_t>& ir_buf) -> bool {
        if (std::string_view::npos == value.find(' ')) {
            encode_normal_str(value, ir_buf);
        } else {
            // TODO: replace this by CLP string encoding
            encode_normal_str(value, ir_buf);
        }
        return true;
    }

    /**
     * Decodes a normal string from the given reader.
     * @param reader
     * @param tag
     * @param value Returns the decoded value.
     * @return IRErrorCode_Success on success
     * @return IRErrorCode_Corrupted_IR if reader contains invalid IR
     * @return IRErrorCode_Incomplete_IR if reader doesn't contain enough data
     */
    auto decode_normal_str(ReaderInterface& reader, int8_t tag, value_t& value) -> IRErrorCode {
        size_t str_length{};
        if (cProtocol::Payload::ValueStrLenUByte == tag) {
            uint8_t length{};
            if (false == decode_int(reader, length)) {
                return IRErrorCode_Incomplete_IR;
            }
            str_length = static_cast<size_t>(length);
        } else if (cProtocol::Payload::ValueStrLenUShort == tag) {
            uint16_t length{};
            if (false == decode_int(reader, length)) {
                return IRErrorCode_Incomplete_IR;
            }
            str_length = static_cast<size_t>(length);
        } else if (cProtocol::Payload::ValueStrLenUInt == tag) {
            uint32_t length{};
            if (false == decode_int(reader, length)) {
                return IRErrorCode_Incomplete_IR;
            }
            str_length = static_cast<size_t>(length);
        } else {
            return IRErrorCode_Corrupted_IR;
        }
        std::string str_value;
        if (ErrorCode_Success != reader.try_read_string(str_length, str_value)) {
            return IRErrorCode_Incomplete_IR;
        }
        value = str_value;
        return IRErrorCode_Success;
    }

    /**
     * Encodes null into IR.
     * @param ir_buf
     * @return true on success.
     */
    auto encode_null(std::vector<int8_t>& ir_buf) -> bool {
        ir_buf.push_back(cProtocol::Payload::ValueNull);
        return true;
    }
}  // namespace

auto Value::encode(std::vector<int8_t>& ir_buf) const -> bool {
    if (is_empty()) {
        return encode_null(ir_buf);
    }

    if (is_type<value_int_t>()) {
        return encode_value_int(get<value_int_t>(), ir_buf);
    } else if (is_type<value_float_t>()) {
        return encode_value_float(get<value_float_t>(), ir_buf);
    } else if (is_type<value_bool_t>()) {
        return encode_value_bool(get<value_bool_t>(), ir_buf);
    } else if (is_type<value_str_t>()) {
        return encode_value_str(get<value_str_t>(), ir_buf);
    }
    return false;
}

auto Value::decode_from_reader(ReaderInterface& reader) -> IRErrorCode {
    int8_t tag{};
    if (ErrorCode_Success != reader.try_read_numeric_value(tag)) {
        return IRErrorCode_Incomplete_IR;
    }
    return decode_from_reader(reader, tag);
}

auto Value::decode_from_reader(ReaderInterface& reader, encoded_tag_t tag) -> IRErrorCode {
    IRErrorCode error_code{IRErrorCode_Success};
    switch (tag) {
        case cProtocol::Payload::ValueInt32:
        case cProtocol::Payload::ValueInt64:
            error_code = decode_value_int(reader, tag, m_value);
            break;
        case cProtocol::Payload::ValueDouble:
            error_code = decode_value_float(reader, tag, m_value);
            break;
        case cProtocol::Payload::ValueTrue:
            m_value = true;
            break;
        case cProtocol::Payload::ValueFalse:
            m_value = false;
            break;
        case cProtocol::Payload::ValueStrLenUByte:
        case cProtocol::Payload::ValueStrLenUShort:
        case cProtocol::Payload::ValueStrLenUInt:
            error_code = decode_normal_str(reader, tag, m_value);
            break;
        case cProtocol::Payload::ValueNull:
            m_value = std::monostate{};
            break;
        default:
            error_code = IRErrorCode_Corrupted_IR;
            break;
    }
    return error_code;
}

auto Value::get_schema_tree_node_type() const -> SchemaTreeNodeValueType {
    if (is_empty()) {
        return SchemaTreeNodeValueType::Obj;
    }

    if (is_type<value_int_t>()) {
        return SchemaTreeNodeValueType::Int;
    } else if (is_type<value_float_t>()) {
        return SchemaTreeNodeValueType::Float;
    } else if (is_type<value_bool_t>()) {
        return SchemaTreeNodeValueType::Bool;
    } else if (is_type<value_str_t>()) {
        return SchemaTreeNodeValueType::Str;
    }

    return SchemaTreeNodeValueType::Unknown;
}

auto Value::dump() const -> std::string {
    if (is_empty()) {
        return "null";
    }

    if (is_type<value_int_t>()) {
        return std::to_string(get<value_int_t>());
    } else if (is_type<value_float_t>()) {
        return std::to_string(get<value_float_t>());
    } else if (is_type<value_bool_t>()) {
        return get<value_bool_t>() ? "True" : "False";
    } else if (is_type<value_str_t>()) {
        return static_cast<std::string>(get<value_str_t>());
    }

    throw ValueException(ErrorCode_Failure, __FILENAME__, __LINE__, "Unknown type");
}

auto Value::convert_from_json(SchemaTreeNodeValueType type, nlohmann::json const& json_val)
        -> Value {
    switch (type) {
        case SchemaTreeNodeValueType::Int:
            return {json_val.get<value_int_t>()};
        case SchemaTreeNodeValueType::Float:
            return {json_val.get<double>()};
        case SchemaTreeNodeValueType::Bool:
            return {json_val.get<bool>()};
        case SchemaTreeNodeValueType::Str:
            return {json_val.get<std::string>()};
        case SchemaTreeNodeValueType::Obj:
            return {};
        default:
            throw ValueException(ErrorCode_BadParam, __FILENAME__, __LINE__, "Unkown type");
    }
    return {};
}
}  // namespace ffi::ir_stream
