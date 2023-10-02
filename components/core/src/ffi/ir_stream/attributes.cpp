#include "attributes.hpp"

#include <stdint.h>

#include <json/single_include/nlohmann/json.hpp>

#include "encoding_methods.hpp"
#include "protocol_constants.hpp"

namespace ffi::ir_stream {
auto Attribute::encode(std::vector<int8_t>& ir_buf) const -> bool {
    auto attr_int_handler = [&]() -> bool {
        auto const int_val{get_value<attr_int_t>()};
        if (INT8_MIN <= int_val && int_val <= INT8_MAX) {
            ir_buf.push_back(cProtocol::Payload::AttrNumByte);
            ir_buf.push_back(static_cast<int8_t>(int_val));
        } else if (INT16_MIN <= int_val && int_val <= INT16_MAX) {
            ir_buf.push_back(cProtocol::Payload::AttrNumShort);
            encode_int(static_cast<int16_t>(int_val), ir_buf);
        } else if (INT32_MIN <= int_val && int_val <= INT32_MAX) {
            ir_buf.push_back(cProtocol::Payload::AttrNumInt);
            encode_int(static_cast<int32_t>(int_val), ir_buf);
        } else if (INT64_MIN <= int_val && int_val <= INT64_MAX) {
            ir_buf.push_back(cProtocol::Payload::AttrNumLong);
            encode_int(static_cast<int64_t>(int_val), ir_buf);
        } else {
            return false;
        }
        return true;
    };

    auto attr_str_handler = [&]() -> bool {
        auto str_val{get_value<attr_str_t>()};
        auto const length{str_val.length()};
        if (length <= UINT8_MAX) {
            ir_buf.push_back(cProtocol::Payload::AttrStrLenByte);
            ir_buf.push_back(bit_cast<int8_t>(static_cast<uint8_t>(length)));
        } else if (length <= UINT16_MAX) {
            ir_buf.push_back(cProtocol::Payload::AttrStrLenShort);
            encode_int(static_cast<uint16_t>(length), ir_buf);
        } else if (length <= INT32_MAX) {
            ir_buf.push_back(cProtocol::Payload::AttrStrLenInt);
            encode_int(static_cast<int32_t>(length), ir_buf);
        } else {
            return false;
        }
        ir_buf.insert(ir_buf.cend(), str_val.cbegin(), str_val.cend());
        return true;
    };

    if (is_type<attr_int_t>()) {
        return attr_int_handler();
    } else if (is_type<attr_str_t>()) {
        return attr_str_handler();
    }
    return false;
}

auto to_json(nlohmann::json& data, AttributeInfo const& attr_info) -> void {
    data = nlohmann::json{
            {AttributeInfo::cNameKey, attr_info.get_name()},
            {AttributeInfo::cTypeTagKey, attr_info.get_type_tag()}
    };
}
}  // namespace ffi::ir_stream
