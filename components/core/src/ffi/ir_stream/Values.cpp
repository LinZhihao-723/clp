#include "Values.hpp"

#include "encoding_methods.hpp"
#include "protocol_constants.hpp"

namespace ffi::ir_stream {
namespace {
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

    auto encode_value_float(value_float_t value, std::vector<int8_t>& ir_buf) -> bool {
        static_assert(std::is_same_v<value_float_t, double>, "Currently only double is supported.");
        ir_buf.push_back(cProtocol::Payload::ValueDouble);
        encode_floating_number(value, ir_buf);
        return true;
    }

    auto encode_value_bool(value_bool_t value, std::vector<int8_t>& ir_buf) -> bool {
        if (value) {
            ir_buf.push_back(cProtocol::Payload::ValueTrue);
        } else {
            ir_buf.push_back(cProtocol::Payload::ValueFalse);
        }
        return true;
    }

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
        return true;
    }

    auto encode_value_str(std::string_view value, std::vector<int8_t>& ir_buf) -> bool {
        if (std::string_view::npos == value.find(' ')) {
            encode_normal_str(value, ir_buf);
        } else {
            // TODO: replace this by CLP string encoding
            encode_normal_str(value, ir_buf);
        }
        return true;
    }
}  // namespace

auto Value::encode(std::vector<int8_t>& ir_buf) const -> bool {
    if (is_empty()) {
        return false;
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

auto Value::get_expected_schema_tree_node_type() const -> SchemaTreeNodeValueType {
    if (is_empty()) {
        return SchemaTreeNodeValueType::Unknown;
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
}  // namespace ffi::ir_stream
