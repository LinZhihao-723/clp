#ifndef CLP_GENERICERRORCODE_HPP
#define CLP_GENERICERRORCODE_HPP

#include <string>
#include <system_error>

namespace clp {
/**
 * Concept for int-based error code enum.
 */
template <typename T>
concept ErrorEnumType = std::is_enum_v<T> && requires(T t) {
    {
        static_cast<std::underlying_type_t<T>>(t)
    } -> std::convertible_to<int>;
};

template <ErrorEnumType ErrorEnum>
class ErrorCategory : public std::error_category {
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

    /**
     * @param The error code encoded in int.
     * @return The descriptive message for the error.
     */
    [[nodiscard]] auto message(ErrorEnum error_enum) const -> std::string;
};

/**
 * Template class for error code. It should take an error code enum and error category.
 */
template <ErrorEnumType ErrorEnum>
class ErrorCode {
public:
    ErrorCode(ErrorEnum error) : m_error{error} {}

    [[nodiscard]] auto get_errno() const -> int;
    [[nodiscard]] auto get_err_enum() const -> ErrorEnum;

    /**
     * Returns the reference of a singleton of the error category.
     */
    [[nodiscard]] static auto get_category() -> ErrorCategory<ErrorEnum> const&;

private:
    static inline ErrorCategory<ErrorEnum> const cCategory{};
    ErrorEnum m_error;
};

template <ErrorEnumType ErrorEnum>
auto ErrorCategory<ErrorEnum>::message(ErrorEnum error_enum) const -> std::string {
    return message(static_cast<int>(error_enum));
}

template <ErrorEnumType ErrorEnum>
auto ErrorCode<ErrorEnum>::get_errno() const -> int {
    return static_cast<int>(m_error);
}

template <ErrorEnumType ErrorEnum>
auto ErrorCode<ErrorEnum>::get_err_enum() const -> ErrorEnum {
    return m_error;
}

template <ErrorEnumType ErrorEnum>
auto ErrorCode<ErrorEnum>::get_category() -> ErrorCategory<ErrorEnum> const& {
    return ErrorCode<ErrorEnum>::cCategory;
}

/**
 * Converts `ErrorCode` to std::error_code.
 */
template <typename ErrorEnum>
[[nodiscard]] auto make_error_code(ErrorCode<ErrorEnum> e) -> std::error_code {
    return {e.get_errno(), ErrorCode<ErrorEnum>::get_category()};
}
}  // namespace clp

#endif  // CLP_GENERICERRORCODE_HPP
