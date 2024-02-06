#ifndef CLP_S_FFI_IR_STREAM_SERIALIZATION_METHODS_HPP
#define CLP_S_FFI_IR_STREAM_SERIALIZATION_METHODS_HPP

#include <msgpack.hpp>

#include "SerializationBuffer.hpp"

namespace clp_s::ffi::ir_stream {
[[nodiscard]] auto
serialize_key_value_pair_records(msgpack::object const& record, SerializationBuffer& ir_buf)
        -> bool;
}  // namespace clp_s::ffi::ir_stream

#endif
