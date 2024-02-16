#ifndef CLP_S_FFI_IR_STREAM_UTILS_HPP
#define CLP_S_FFI_IR_STREAM_UTILS_HPP

#include <msgpack.hpp>
#include <string>

namespace clp_s::ffi::ir_stream {
/**
 * Appends a msgpack encoded array into a json string.
 * @param array
 * @param json_str
 * @return true on success, false on failure.
 */
[[nodiscard]] auto
append_msgpack_array_to_json_str(msgpack::object const& array, std::string& json_str) -> bool;

/**
 * Appends a msgpack encoded map into a json string.
 * @param map
 * @param json_str
 * @return true on success, false on failure.
 */
[[nodiscard]] auto append_msgpack_map_to_json_str(msgpack::object const& map, std::string& json_str)
        -> bool;
}  // namespace clp_s::ffi::ir_stream

#endif
