#ifndef FFI_IR_STREAM_DECODING_METHODS_TPP
#define FFI_IR_STREAM_DECODING_METHODS_TPP

#include <string>
#include <vector>

#include "../../ir/parsing.hpp"
#include "../encoding_methods.hpp"
#include "byteswap.hpp"
#include "decoding_methods.hpp"
#include "protocol_constants.hpp"

namespace ffi::ir_stream {
template <typename integer_t>
bool decode_int(ReaderInterface& reader, integer_t& value) {
    integer_t value_little_endian;
    if (reader.try_read_numeric_value(value_little_endian) != ErrorCode_Success) {
        return false;
    }

    constexpr auto read_size = sizeof(integer_t);
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

template <typename float_t>
bool decode_floating_number(ReaderInterface& reader, float_t& value) {
    constexpr auto read_size{sizeof(value)};
    static_assert(4 == read_size || 8 == read_size);
    if constexpr (4 == read_size) {
        int32_t bit_representation{};
        if (false == decode_int(reader, bit_representation)) {
            return false;
        }
        value = bit_cast<float_t>(bit_representation);
    } else if constexpr (8 == read_size) {
        int64_t bit_representation{};
        if (false == decode_int(reader, bit_representation)) {
            return false;
        }
        value = bit_cast<float_t>(bit_representation);
    }
    return true;
}

template <
        bool unescape_logtype,
        typename encoded_variable_t,
        typename ConstantHandler,
        typename EncodedIntHandler,
        typename EncodedFloatHandler,
        typename DictVarHandler>
void generic_decode_message(
        std::string const& logtype,
        std::vector<encoded_variable_t> const& encoded_vars,
        std::vector<std::string> const& dict_vars,
        ConstantHandler constant_handler,
        EncodedIntHandler encoded_int_handler,
        EncodedFloatHandler encoded_float_handler,
        DictVarHandler dict_var_handler
) {
    auto const logtype_length = logtype.length();
    auto const encoded_vars_length = encoded_vars.size();
    auto const dict_vars_length = dict_vars.size();
    size_t next_static_text_begin_pos = 0;

    size_t dictionary_vars_ix = 0;
    size_t encoded_vars_ix = 0;
    for (size_t cur_pos = 0; cur_pos < logtype_length; ++cur_pos) {
        auto c = logtype[cur_pos];
        switch (c) {
            case enum_to_underlying_type(VariablePlaceholder::Float): {
                constant_handler(
                        logtype,
                        next_static_text_begin_pos,
                        cur_pos - next_static_text_begin_pos
                );
                next_static_text_begin_pos = cur_pos + 1;
                if (encoded_vars_ix >= encoded_vars_length) {
                    throw DecodingException(
                            ErrorCode_Corrupt,
                            __FILENAME__,
                            __LINE__,
                            cTooFewEncodedVarsErrorMessage
                    );
                }
                encoded_float_handler(encoded_vars[encoded_vars_ix]);
                ++encoded_vars_ix;

                break;
            }

            case enum_to_underlying_type(VariablePlaceholder::Integer): {
                constant_handler(
                        logtype,
                        next_static_text_begin_pos,
                        cur_pos - next_static_text_begin_pos
                );
                next_static_text_begin_pos = cur_pos + 1;
                if (encoded_vars_ix >= encoded_vars_length) {
                    throw DecodingException(
                            ErrorCode_Corrupt,
                            __FILENAME__,
                            __LINE__,
                            cTooFewEncodedVarsErrorMessage
                    );
                }
                encoded_int_handler(encoded_vars[encoded_vars_ix]);
                ++encoded_vars_ix;

                break;
            }

            case enum_to_underlying_type(VariablePlaceholder::Dictionary): {
                constant_handler(
                        logtype,
                        next_static_text_begin_pos,
                        cur_pos - next_static_text_begin_pos
                );
                next_static_text_begin_pos = cur_pos + 1;
                if (dictionary_vars_ix >= dict_vars_length) {
                    throw DecodingException(
                            ErrorCode_Corrupt,
                            __FILENAME__,
                            __LINE__,
                            cTooFewDictionaryVarsErrorMessage
                    );
                }
                dict_var_handler(dict_vars[dictionary_vars_ix]);
                ++dictionary_vars_ix;

                break;
            }

            case enum_to_underlying_type(ir::VariablePlaceholder::Escape): {
                // Ensure the escape character is followed by a
                // character that's being escaped
                if (cur_pos == logtype_length - 1) {
                    throw DecodingException(
                            ErrorCode_Corrupt,
                            __FILENAME__,
                            __LINE__,
                            cUnexpectedEscapeCharacterMessage
                    );
                }

                if constexpr (unescape_logtype) {
                    constant_handler(
                            logtype,
                            next_static_text_begin_pos,
                            cur_pos - next_static_text_begin_pos
                    );

                    // Skip the escape character
                    next_static_text_begin_pos = cur_pos + 1;
                }
                // The character after the escape character is static text
                // (regardless of whether it is a variable placeholder), so
                // increment cur_pos by 1 to ensure we don't process the
                // next character in any of the other cases (instead it will
                // be added to the message).
                ++cur_pos;

                break;
            }
        }
    }
    // Add remainder
    if (next_static_text_begin_pos < logtype_length) {
        constant_handler(
                logtype,
                next_static_text_begin_pos,
                logtype_length - next_static_text_begin_pos
        );
    }
}
}  // namespace ffi::ir_stream

#endif  // FFI_IR_STREAM_DECODING_METHODS_TPP
