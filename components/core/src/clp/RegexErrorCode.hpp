#ifndef CLP_REGEXERRORCODE_HPP
#define CLP_REGEXERRORCODE_HPP

#include <cstdint>
#include <string>
#include <system_error>

#include "GenericErrorCode.hpp"

namespace clp {
enum class RegexErrorEnum : uint8_t {
    Success = 0,
    IllegalState,
    Star,
    Plus,
    Question,
    Pipe,
    Caret,
    Dollar,
    DisallowedEscapeSequence,
    UnmatchedParenthesis,
    UnsupportedCharsets,
    IncompleteCharsetStructure,
    UnsupportedQuantifier,
    TokenUnquantifiable,
};

using RegexErrorCategory = ErrorCategory<RegexErrorEnum>;
using RegexErrorCode = ErrorCode<RegexErrorEnum>;
}  // namespace clp

namespace std {
template <>
struct is_error_code_enum<clp::RegexErrorCode> : std::true_type {};
}  // namespace std

#endif  // CLP_REGEXERRORCODE_HPP
