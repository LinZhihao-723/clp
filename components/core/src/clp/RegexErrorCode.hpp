//
// Created by pleia on 7/17/2024.
//

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

class RegexErrorCategory : public std::error_category {
public:
    /**
     * @return The class of errors.
     */
    [[nodiscard]] auto name() const noexcept -> char const* override;

    /**
     * @param The error code encoded in int.
     * @return The descriptive message for the error.
     */
    [[nodiscard]] auto message(int ev) const -> std::string override;
};

using RegexErrorCode = ErrorCode<RegexErrorEnum, RegexErrorCategory>;
}  // namespace clp

namespace std {
template <>
struct is_error_code_enum<clp::RegexErrorCode> : std::true_type {};
}  // namespace std

#endif  // CLP_REGEXERRORCODE_HPP
