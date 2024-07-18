#include "RegexErrorCode.hpp"

#include <string>

#include "GenericErrorCode.hpp"

namespace clp {
template <>
auto RegexErrorCategory::name() const noexcept -> char const* {
    return "regex utility";
}

template <>
auto RegexErrorCategory::message(int ev) const -> std::string {
    switch (static_cast<RegexErrorEnum>(ev)) {
        case RegexErrorEnum::Success:
            return "Success.";

        case RegexErrorEnum::IllegalState:
            return "Unrecognized state.";

        case RegexErrorEnum::Star:
            return "Failed to translate due to metachar `*` (zero or more occurences).";

        case RegexErrorEnum::Plus:
            return "Failed to translate due to metachar `+` (one or more occurences).";

        case RegexErrorEnum::Question:
            return "Currently does not support returning a list of wildcard translations. The "
                   "metachar `?` (lazy match) may be supported in the future.";

        case RegexErrorEnum::Pipe:
            return "Currently does not support returning a list of wildcard translations. The "
                   "regex OR condition feature may be supported in the future.";

        case RegexErrorEnum::Caret:
            return "Failed to translate due to start anchor `^` in the middle of the string.";

        case RegexErrorEnum::Dollar:
            return "Failed to translate due to end anchor `$` in the middle of the string.";

        case RegexErrorEnum::UnmatchedParenthesis:
            return "Unmatched opening `(` or closing `)`.";

        default:
            return "(unrecognized error)";
    }
}

template class ErrorCategory<RegexErrorEnum>;
template class ErrorCode<RegexErrorEnum>;
}  // namespace clp