#ifndef FFI_IR_STREAM_ENCODING_METHODS_INC
#define FFI_IR_STREAM_ENCODING_METHODS_INC

#include <vector>

#include "../../type_utils.hpp"
#include "byteswap.hpp"

namespace ffi::ir_stream {
template <typename integer_t>
void encode_int(integer_t value, std::vector<int8_t>& ir_buf) {
    integer_t value_big_endian;
    static_assert(sizeof(integer_t) == 2 || sizeof(integer_t) == 4 || sizeof(integer_t) == 8);
    if constexpr (sizeof(value) == 2) {
        value_big_endian = bswap_16(value);
    } else if constexpr (sizeof(value) == 4) {
        value_big_endian = bswap_32(value);
    } else if constexpr (sizeof(value) == 8) {
        value_big_endian = bswap_64(value);
    }
    auto data = reinterpret_cast<int8_t*>(&value_big_endian);
    ir_buf.insert(ir_buf.end(), data, data + sizeof(value));
}

template <typename float_t>
void encode_floating_number(float_t value, std::vector<int8_t>& ir_buf) {
    static_assert(std::is_same_v<float_t, double> || std::is_same_v<float_t, float>);
    if constexpr (sizeof(value) == 4) {
        encode_int(bit_cast<int32_t>(value), ir_buf);
    } else {
        static_assert(sizeof(value) == 8);
        encode_int(bit_cast<int64_t>(value), ir_buf);
    }
}
}  // namespace ffi::ir_stream

#endif
