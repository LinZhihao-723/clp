#ifndef CLP_S_FFI_IR_STREAM_DESERIALIZATION_METHODS_HPP
#define CLP_S_FFI_IR_STREAM_DESERIALIZATION_METHODS_HPP

#include <optional>
#include <vector>

#include "../../../clp/ReaderInterface.hpp"
#include "../../TraceableException.hpp"
#include "../SchemaTree.hpp"
#include "Value.hpp"

namespace clp_s::ffi::ir_stream {
enum class IRErrorCode : uint8_t {
    Success = 0,
    DecodeError,
    EndOfStream,
    CorruptedStream,
    IncompleteStream,
    NotImplemented,
    UnknownTag
};

class DeserializingException : public TraceableException {
public:
    DeserializingException(
            ErrorCode error_code,
            char const* const filename,
            int line_number,
            std::string message
    )
            : TraceableException(error_code, filename, line_number),
              m_message(std::move(message)) {}

    [[nodiscard]] auto what() const noexcept -> char const* override { return m_message.c_str(); }

private:
    std::string m_message;
};

/**
 * Deserializes the next key-value pair record from the reader. The record is represented as the
 * schema and deserialized values.
 * @param reader
 * @param schema_tree
 * @param schema Outputs the deserialized schema.
 * @param values Outputs the deserialized values.
 * @param IRErrorCode::Success on success.
 * @param IRErrorCode::DecodeError if the encoded values cannot be properly deserialized.
 * @param IRErrorCode::EndOfStream if the IR ends.
 * @param IRErrorCode::CorruptedStream if the stream contains invalid byte sequence.
 * @param IRErrorCode::IncompleteStream if the reader doesn't contain enough data to deserialize a
 * record.
 * @param IRErrorCode::UnknownTag if the streaming contains unknown header bytes.
 */
[[nodiscard]] auto deserialize_next_key_value_pair_record(
        clp::ReaderInterface& reader,
        SchemaTree& schema_tree,
        std::vector<SchemaTreeNode::id_t>& schema,
        std::vector<std::optional<Value>>& values
) -> IRErrorCode;

/**
 * Deserializes the key-value pair record as a JSON string.
 * @param schema_tree
 * @param schema The schema of the record.
 * @param values The values of the record.
 * @param json_str Outputs the deserialized JSON string.
 * @return true on success, false otherwise.
 */
[[nodiscard]] auto deserialize_record_as_json_str(
        SchemaTree const& schema_tree,
        std::vector<SchemaTreeNode::id_t> const& schema,
        std::vector<std::optional<Value>> const& values,
        std::string& json_str
) -> bool;
}  // namespace clp_s::ffi::ir_stream

#endif
