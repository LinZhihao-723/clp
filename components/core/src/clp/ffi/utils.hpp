#ifndef CLP_FFI_UTILS_HPP
#define CLP_FFI_UTILS_HPP

#include <optional>
#include <string>
#include <string_view>

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
}  // namespace clp::ffi

#endif  // CLP_FFI_UTILS_HPP
