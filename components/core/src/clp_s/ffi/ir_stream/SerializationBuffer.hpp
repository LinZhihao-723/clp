#ifndef CLP_S_FFI_IR_STREAM_SERIALIZATION_BUFFER
#define CLP_S_FFI_IR_STREAM_SERIALIZATION_BUFFER

#include <msgpack.hpp>
#include <vector>

#include <span>

#include "../SchemaTree.hpp"

namespace clp_s::ffi::ir_stream {
/**
 * When serializing log records into an IR stream, the serializer will maintain internal data
 * structures, such as the schema tree and multiple internal buffers. This class wraps all the
 * necessary internal data structures of a serialized CLP IR stream to provide better abstraction
 * and improve buffer locality. Each IR stream should maintain its own serialization buffer.
 */
class SerializationBuffer {
public:
    /**
     * Gets the IR buffer that contains the serialized CLP IR byte sequence.
     * @return The current ir buf as a span.
     */
    [[nodiscard]] auto get_ir_buf() -> std::span<char const> {
        // TODO: we should replace this by either std::span or std::span (c++20).
        return {reinterpret_cast<char const*>(m_ir_buf.data()), m_ir_buf.size()};
    }

    [[nodiscard]] auto get_schema_tree() const -> SchemaTree const& {
        return m_schema_tree;
    }

    /**
     * Flush the current IR buffer.
     */
    auto flush_ir_buf() -> void { m_ir_buf.clear(); }

    /**
     * Resets all the internal data structures.
     */
    auto reset_all() -> void {
        m_ir_buf.clear();
        m_schema_tree_node_buf.clear();
        m_key_group_buf.clear();
        m_value_group_buf.clear();
        m_schema_tree.reset();
    }

private:
    friend auto serialize_key_value_pair_record(msgpack::object const&, SerializationBuffer&)
            -> bool;
    friend auto serialize_end_of_stream(SerializationBuffer&) -> void;

    std::vector<int8_t> m_ir_buf;
    std::vector<int8_t> m_schema_tree_node_buf;
    std::vector<int8_t> m_key_group_buf;
    std::vector<int8_t> m_value_group_buf;
    SchemaTree m_schema_tree;
};
}  // namespace clp_s::ffi::ir_stream

#endif
