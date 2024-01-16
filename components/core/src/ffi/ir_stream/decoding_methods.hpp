#ifndef FFI_IR_STREAM_DECODING_METHODS_HPP
#define FFI_IR_STREAM_DECODING_METHODS_HPP

#include <string>
#include <vector>

#include "../../ReaderInterface.hpp"
#include "../encoding_methods.hpp"

namespace ffi::ir_stream {
using encoded_tag_t = int8_t;

typedef enum {
    IRErrorCode_Success,
    IRErrorCode_Decode_Error,
    IRErrorCode_Eof,
    IRErrorCode_Corrupted_IR,
    IRErrorCode_Incomplete_IR,
} IRErrorCode;

typedef enum {
    IRProtocolErrorCode_Supported,
    IRProtocolErrorCode_Too_Old,
    IRProtocolErrorCode_Too_New,
    IRProtocolErrorCode_Invalid,
} IRProtocolErrorCode;

class DecodingException : public TraceableException {
public:
    // Constructors
    DecodingException(
            ErrorCode error_code,
            char const* const filename,
            int line_number,
            std::string message
    )
            : TraceableException(error_code, filename, line_number),
              m_message(std::move(message)) {}

    // Methods
    [[nodiscard]] char const* what() const noexcept override { return m_message.c_str(); }

private:
    std::string m_message;
};

/**
 * Decodes an integer from the given reader
 * @tparam integer_t Type of the integer to decode
 * @param reader
 * @param value Returns the decoded integer
 * @return true on success, false if the reader doesn't contain enough data to
 * decode
 */
template <typename integer_t>
bool decode_int(ReaderInterface& reader, integer_t& value);

/**
 * Decodes a non-CLP-encoded floating point from the given reader.
 * @tparam float_t Type of the floating number.
 * @param reader
 * @param value Returns the decoded float.
 * @return true on success, false if the reader doesn't contain enough data to
 * decode.
 */
template <typename float_t>
bool decode_floating_number(ReaderInterface& reader, float_t& value);

/**
 * Decodes the encoding type for the encoded IR stream
 * @param reader
 * @param is_four_bytes_encoding Returns the encoding type
 * @return ErrorCode_Success on success
 * @return ErrorCode_Corrupted_IR if reader contains invalid IR
 * @return ErrorCode_Incomplete_IR if reader doesn't contain enough data to
 * decode
 */
IRErrorCode get_encoding_type(ReaderInterface& reader, bool& is_four_bytes_encoding);

/**
 * Deserializes an IR message from the given stream
 * @tparam encoded_variable_t
 * @param reader
 * @param logtype Returns the logtype
 * @param encoded_vars Returns the encoded variables
 * @param dict_vars Returns the dictionary variables
 * @param timestamp_or_timestamp_delta Returns the timestamp (in the eight-byte
 * encoding case) or the timestamp delta (in the four-byte encoding case)
 * @return IRErrorCode_Success on success
 * @return IRErrorCode_Corrupted_IR if reader contains invalid IR
 * @return IRErrorCode_Incomplete_IR if reader doesn't contain enough data
 * @return IRErrorCode_Eof on reaching the end of the stream
 */
template <typename encoded_variable_t>
auto deserialize_ir_message(
        ReaderInterface& reader,
        std::string& logtype,
        std::vector<encoded_variable_t>& encoded_vars,
        std::vector<std::string>& dict_vars,
        epoch_time_ms_t& timestamp_or_timestamp_delta
) -> IRErrorCode;

/**
 * Decodes the IR message calls the given methods to handle each component of
 * the message
 * @tparam unescape_logtype Whether to remove the escape characters from the
 * logtype before calling \p ConstantHandler
 * @tparam encoded_variable_t Type of the encoded variable
 * @tparam ConstantHandler Method to handle constants in the logtype.
 * Signature: (const std::string&, size_t, size_t) -> void
 * @tparam EncodedIntHandler Method to handle encoded integers.
 * Signature: (encoded_variable_t) -> void
 * @tparam EncodedFloatHandler Method to handle encoded floats.
 * Signature: (encoded_variable_t) -> void
 * @tparam DictVarHandler Method to handle dictionary variables.
 * Signature: (const std::string&) -> void
 * @param logtype
 * @param encoded_vars
 * @param dict_vars
 * @param constant_handler
 * @param encoded_int_handler
 * @param encoded_float_handler
 * @param dict_var_handler
 * @throw DecodingException if the message can not be decoded properly
 */
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
);

/**
 * Decodes the CLP IR encoded string from the reader.
 * @tparam encoded_variable_t
 * @param reader
 * @param clp_str
 * @return IRErrorCode_Success on success
 * @return IRErrorCode_Corrupted_IR if reader contains invalid IR
 * @return IRErrorCode_Incomplete_IR if reader doesn't contain enough data
 * @return IRErrorCode_Eof on reaching the end of the stream
 */
template <typename encoded_variable_t>
auto decode_clp_string(ReaderInterface& reader, std::string& clp_str) -> IRErrorCode;

/**
 * Decodes the preamble for an IR stream.
 * @param reader
 * @param metadata_type Returns the type of the metadata found in the IR
 * @param metadata_pos Returns the starting position of the metadata in reader
 * @param metadata_size Returns the size of the metadata written in the IR
 * @return IRErrorCode_Success on success
 * @return IRErrorCode_Corrupted_IR if reader contains invalid IR
 * @return IRErrorCode_Incomplete_IR if reader doesn't contain enough data to
 * decode
 */
IRErrorCode decode_preamble(
        ReaderInterface& reader,
        encoded_tag_t& metadata_type,
        size_t& metadata_pos,
        uint16_t& metadata_size
);

/**
 * Decodes the preamble for an IR stream.
 * @param reader
 * @param metadata_type Returns the type of the metadata found in the IR
 * @param metadata Returns the metadata in the given vector
 * @return IRErrorCode_Success on success
 * @return IRErrorCode_Corrupted_IR if reader contains invalid IR
 * @return IRErrorCode_Incomplete_IR if reader doesn't contain enough
 * data to decode
 */
IRErrorCode decode_preamble(
        ReaderInterface& reader,
        encoded_tag_t& metadata_type,
        std::vector<int8_t>& metadata
);

/**
 * Validates whether the given protocol version can be supported by the current
 * build.
 * @param protocol_version
 * @return IRProtocolErrorCode_Supported if the protocol version is supported.
 * @return IRProtocolErrorCode_Too_Old if the protocol version is no longer
 * supported by this build's protocol version.
 * @return IRProtocolErrorCode_Too_New if the protocol version is newer than this
 * build's protocol version.
 * @return IRProtocolErrorCode_Invalid if the protocol version does not follow
 * the SemVer specification.
 */
IRProtocolErrorCode validate_protocol_version(std::string_view protocol_version);

namespace eight_byte_encoding {
    /**
     * Decodes the next message for the eight-byte encoding IR stream.
     * @param reader
     * @param message Returns the decoded message
     * @param timestamp Returns the decoded timestamp
     * @return ErrorCode_Success on success
     * @return ErrorCode_Corrupted_IR if reader contains invalid IR
     * @return ErrorCode_Decode_Error if the encoded message cannot be properly
     * decoded
     * @return ErrorCode_Incomplete_IR if reader doesn't contain enough data to
     * decode
     * @return ErrorCode_End_of_IR if the IR ends
     */
    IRErrorCode
    decode_next_message(ReaderInterface& reader, std::string& message, epoch_time_ms_t& timestamp);

    /**
     * Decodes the CLP IR encoded string from the reader.
     * @param reader
     * @param clp_str
     * @return ErrorCode_Success on success
     * @return ErrorCode_Corrupted_IR if reader contains invalid IR
     * @return ErrorCode_Decode_Error if the encoded message cannot be properly
     * decoded
     * @return ErrorCode_Incomplete_IR if reader doesn't contain enough data to
     * decode
     * @return ErrorCode_End_of_IR if the IR ends
     */
    auto decode_clp_str(ReaderInterface& reader, std::string& clp_str) -> IRErrorCode;
}  // namespace eight_byte_encoding

namespace four_byte_encoding {
    /**
     * Decodes the next message for the four-byte encoding IR stream.
     * @param reader
     * @param message Returns the decoded message
     * @param timestamp_delta Returns the decoded timestamp delta
     * @return ErrorCode_Success on success
     * @return ErrorCode_Corrupted_IR if reader contains invalid IR
     * @return ErrorCode_Decode_Error if the encoded message cannot be properly
     * decoded
     * @return ErrorCode_Incomplete_IR if reader doesn't contain enough data to
     * decode
     * @return ErrorCode_End_of_IR if the IR ends
     */
    IRErrorCode decode_next_message(
            ReaderInterface& reader,
            std::string& message,
            epoch_time_ms_t& timestamp_delta
    );

    /**
     * Decodes the CLP IR encoded string from the reader.
     * @param reader
     * @param clp_str
     * @return ErrorCode_Success on success
     * @return ErrorCode_Corrupted_IR if reader contains invalid IR
     * @return ErrorCode_Decode_Error if the encoded message cannot be properly
     * decoded
     * @return ErrorCode_Incomplete_IR if reader doesn't contain enough data to
     * decode
     * @return ErrorCode_End_of_IR if the IR ends
     */
    auto decode_clp_str(ReaderInterface& reader, std::string& clp_str) -> IRErrorCode;
}  // namespace four_byte_encoding
}  // namespace ffi::ir_stream

#include "decoding_methods.inc"

#endif  // FFI_IR_STREAM_DECODING_METHODS_HPP
