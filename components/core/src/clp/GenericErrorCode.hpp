//
// Created by pleia on 7/17/2024.
//

#ifndef CLP_GENERICERRORCODE_HPP
#define CLP_GENERICERRORCODE_HPP

#include <system_error>

namespace clp {
/**
 * Concept for int-based error code enum.
 */
template <typename T>
concept ErrorEnumType = requires(T t) {
    {
        static_cast<int>(t)
    } -> std::same_as<int>;
};

/**
 * Concept for inherited error category.
 */
template <typename T>
concept ErrorCategoryType = std::is_base_of<std::error_category, T>::value;

/**
 * Template class for error code. It should take an error code enum and error category.
 */
template <ErrorEnumType ErrorEnum, ErrorCategoryType ErrorCategory>
class ErrorCode {
public:
    ErrorCode(ErrorEnum error) : m_error{error} {}

    [[nodiscard]] auto get_errno() const -> int;
    [[nodiscard]] auto get_err_enum() const -> ErrorEnum;

    /**
     * Returns the reference of a singleton of the error category.
     */
    [[nodiscard]] static auto get_category() -> ErrorCategory const&;

private:
    ErrorEnum m_error;
};

template <ErrorEnumType ErrorEnum, ErrorCategoryType ErrorCategory>
auto ErrorCode<ErrorEnum, ErrorCategory>::get_errno() const -> int {
    return static_cast<int>(m_error);
}

template <ErrorEnumType ErrorEnum, ErrorCategoryType ErrorCategory>
auto ErrorCode<ErrorEnum, ErrorCategory>::get_err_enum() const -> ErrorEnum {
    return m_error;
}

/**
 * Converts `ErrorCode` to std::error_code.
 */
template <typename ErrorEnum, typename ErrorCategory>
[[nodiscard]] auto make_error_code(ErrorCode<ErrorEnum, ErrorCategory> e) -> std::error_code {
    return {e.get_errno(), ErrorCode<ErrorEnum, ErrorCategory>::get_category()};
}
}  // namespace clp

#endif  // CLP_GENERICERRORCODE_HPP
