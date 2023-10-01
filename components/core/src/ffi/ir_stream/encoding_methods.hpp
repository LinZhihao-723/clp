#ifndef FFI_IR_STREAM_ENCODING_METHODS_HPP
#define FFI_IR_STREAM_ENCODING_METHODS_HPP

#include <string_view>
#include <vector>

#include "../encoding_methods.hpp"
#include "attributes.hpp"
#include "byteswap.h"

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

namespace eight_byte_encoding {
    /**
     * Encodes the preamble for the eight-byte encoding IR stream
     * @param timestamp_pattern
     * @param timestamp_pattern_syntax
     * @param time_zone_id
     * @param ir_buf
     * @return true on success, false otherwise
     */
    bool encode_preamble(
            std::string_view timestamp_pattern,
            std::string_view timestamp_pattern_syntax,
            std::string_view time_zone_id,
            std::vector<int8_t>& ir_buf
    );

    /**
     * Encodes the given message into the eight-byte encoding IR stream
     * @param timestamp
     * @param message
     * @param logtype
     * @param ir_buf
     * @return true on success, false otherwise
     */
    bool encode_message(
            epoch_time_ms_t timestamp,
            std::string_view message,
            std::string& logtype,
            std::vector<int8_t>& ir_buf
    );
}  // namespace eight_byte_encoding

namespace four_byte_encoding {
    /**
     * Encodes the preamble for the four-byte encoding IR stream
     * @param timestamp_pattern
     * @param timestamp_pattern_syntax
     * @param time_zone_id
     * @param reference_timestamp
     * @param ir_buf
     * @return true on success, false otherwise
     */
    bool encode_preamble(
            std::string_view timestamp_pattern,
            std::string_view timestamp_pattern_syntax,
            std::string_view time_zone_id,
            epoch_time_ms_t reference_timestamp,
            std::vector<int8_t>& ir_buf
    );

    /**
     * Encodes the preamble for the four-byte encoding IR stream
     * @param timestamp_pattern
     * @param timestamp_pattern_syntax
     * @param time_zone_id
     * @param reference_timestamp
     * @param attribute_table
     * @param ir_buf
     * @return true on success, false otherwise
     */
    bool encode_preamble(
            std::string_view timestamp_pattern,
            std::string_view timestamp_pattern_syntax,
            std::string_view time_zone_id,
            epoch_time_ms_t reference_timestamp,
            std::vector<AttributeInfo> const& attribute_table,
            std::vector<int8_t>& ir_buf
    );

    /**
     * Encodes the given message into the four-byte encoding IR stream
     * @param timestamp_delta
     * @param message
     * @param logtype
     * @param ir_buf
     * @return true on success, false otherwise
     */
    bool encode_message(
            epoch_time_ms_t timestamp_delta,
            std::string_view message,
            std::string& logtype,
            std::vector<int8_t>& ir_buf
    );

    /**
     * Encodes the given message into the four-byte encoding IR stream
     * @param timestamp_delta
     * @param message
     * @param logtype
     * @param ir_buf
     * @return true on success, false otherwise
     */
    bool encode_message(
            epoch_time_ms_t timestamp_delta,
            std::string_view message,
            std::string& logtype,
            std::vector<std::optional<Attribute>> const& attributes,
            std::vector<int8_t>& ir_buf
    );

    /**
     * Encodes the given message into the four-byte encoding IR stream without
     * encoding timestamp delta
     * @param message
     * @param logtype
     * @param ir_buf
     * @return true on success, false otherwise
     */
    bool
    encode_message(std::string_view message, std::string& logtype, std::vector<int8_t>& ir_buf);

    /**
     * Encodes the given timestamp delta into the four-byte encoding IR stream
     * @param timestamp_delta
     * @param ir_buf
     * @return true on success, false otherwise
     */
    bool encode_timestamp(epoch_time_ms_t timestamp_delta, std::vector<int8_t>& ir_buf);
}  // namespace four_byte_encoding
}  // namespace ffi::ir_stream

#endif  // FFI_IR_STREAM_ENCODING_METHODS_HPP
