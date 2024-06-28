#ifndef CLP_FFI_UTILS_HPP
#define CLP_FFI_UTILS_HPP

#include <optional>
#include <string>
#include <string_view>

#include <msgpack.hpp>

namespace clp::ffi {
/**
 * Validates whether the given string is UTF-8 encoded, and escapes any characters to make the
 * string compatible with the JSON specification.
 * @param raw The raw string to escape.
 * @return The escaped string on success.
 * @return std::nullopt if the string contains any non-UTF-8-encoded byte sequences.
 */
[[nodiscard]] auto validate_and_escape_utf8_string(std::string_view raw
) -> std::optional<std::string>;

/**
 * Validates whether `src` is UTF-8 encoded, and appends `src` to `dst` while escaping any
 * characters to make the appended string compatible with the JSON specification.
 * @param src The string to validate and escape.
 * @param dst Returns `dst` with an escaped version of `src` appended.
 * @return Whether `src` is a valid UTF-8-encoded string. NOTE: Even if `src` is not UTF-8 encoded,
 * `dst` may be modified.
 */
[[nodiscard]] auto
validate_and_append_escaped_utf8_string(std::string_view src, std::string& dst) -> bool;

/**
 * Serializes and appends a msgpack array to the given JSON string.
 * @param array
 * @param json_str Outputs the appended JSON string.
 * @return Whether the serialized succeeded. NOTE: Event if the serialization failed, `json_str` may
 * be modified.
 */
[[nodiscard]] auto serialize_and_append_msgpack_array_to_json_str(
        msgpack::object const& array,
        std::string& json_str
) -> bool;

/**
 * Serializes and appends a msgpack map to the given JSON string.
 * @param map
 * @param json_str Outputs the appended JSON string.
 * @return Whether the serialized succeeded. NOTE: Event if the serialization failed, `json_str` may
 * be modified.
 */
[[nodiscard]] auto serialize_and_append_msgpack_map_to_json_str(
        msgpack::object const& map,
        std::string& json_str
) -> bool;
}  // namespace clp::ffi

#endif  // CLP_FFI_UTILS_HPP
