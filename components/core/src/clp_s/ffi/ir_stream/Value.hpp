#ifndef CLP_S_FFI_IR_STREAM_VALUE_HPP
#define CLP_S_FFI_IR_STREAM_VALUE_HPP

#include <stdint.h>

#include <string>
#include <tuple>
#include <type_traits>
#include <variant>
#include <vector>

#include <json/single_include/nlohmann/json.hpp>

#include "../../TraceableException.hpp"
#include "ClpString.hpp"

namespace clp_s::ffi::ir_stream {
using value_int_t = int64_t;
using value_float_t = double;
using value_bool_t = bool;
using value_str_t = std::string;
using value_eight_byte_clp_str_t = EightByteEncodingClpString;
using value_four_byte_clp_str_t = FourByteEncodingClpString;

/**
 * Template struct that converts a type to an immutable return type.
 * @tparam value_type
 */
template <typename value_type>
struct ImmutableReturnTypeConverter {
    using type = value_type;
};

/**
 * Template struct that represents the immutable return type of the given value_type.
 * @tparam value_type
 */
template <typename value_type>
using ImmutableReturnType = typename ImmutableReturnTypeConverter<value_type>::type;

template <>
struct ImmutableReturnTypeConverter<value_str_t> {
    using type = std::string_view;
};

template <>
struct ImmutableReturnTypeConverter<value_eight_byte_clp_str_t> {
    using type = value_eight_byte_clp_str_t const&;
};

template <>
struct ImmutableReturnTypeConverter<value_four_byte_clp_str_t> {
    using type = value_four_byte_clp_str_t const&;
};

/**
 * Template struct that represents a value type identity.
 * @tparam value_type
 */
template <typename value_type>
struct value_type_identity {
    using type = value_type;
};

/**
 * A tuple that contains all the valid value types.
 */
using valid_value_types = std::tuple<
        value_type_identity<value_int_t>,
        value_type_identity<value_float_t>,
        value_type_identity<value_bool_t>,
        value_type_identity<value_str_t>,
        value_type_identity<value_eight_byte_clp_str_t>,
        value_type_identity<value_four_byte_clp_str_t>>;

/**
 * Templates to enum the tuple to create variant.
 * @tparam value_type
 */
template <typename value_type, typename...>
struct enum_value_types_to_variant;

template <typename... value_types>
struct enum_value_types_to_variant<std::tuple<value_types...>> {
    using type = std::variant<std::monostate, typename value_types::type...>;
};

/**
 * A super type of all the valid value types.
 *     Int: value_int_t (int64_t)
 *     FLoat: value_float_t (double)
 *     Boolean: value_bool_t (bool)
 *     String: value_str_t (String)
 *     ClpString(FourByteEncoding): value_four_byte_clp_str_t
 *     ClpString(EightByteEncoding): value_eight_byte_clp_str_t
 *     Null: std::monostate
 */
using value_t = enum_value_types_to_variant<valid_value_types>::type;

/**
 * Templates for value type validations.
 * @tparam value_type
 */
template <typename type_to_validate, typename Tuple>
struct is_valid_type;

template <typename type_to_validate>
struct is_valid_type<type_to_validate, std::tuple<>> : std::false_type {};

template <typename type_to_validate, typename type_unknown, typename... types_to_validate>
struct is_valid_type<type_to_validate, std::tuple<type_unknown, types_to_validate...>>
        : is_valid_type<type_to_validate, std::tuple<types_to_validate...>> {};

template <typename type_to_validate, typename... types_to_validate>
struct is_valid_type<type_to_validate, std::tuple<type_to_validate, types_to_validate...>>
        : std::true_type {};

/**
 * A template to validate whether a given type is a valid value type.
 */
template <typename type_to_validate>
constexpr bool is_valid_value_type{is_valid_type<type_to_validate, valid_value_types>::value};

/**
 * This class wraps the super type for valid value types, and provides necessary methods to
 * construct/access a value.
 */
class Value {
public:
    class ValueException : public TraceableException {
    public:
        ValueException(
                ErrorCode error_code,
                char const* const filename,
                int line_number,
                std::string message
        )
                : TraceableException{error_code, filename, line_number},
                  m_message{std::move(message)} {}

        [[nodiscard]] auto what() const noexcept -> char const* override {
            return m_message.c_str();
        }

    private:
        std::string m_message;
    };

    Value() = default;
    ~Value() = default;
    Value(Value const&) = default;
    auto operator=(Value const&) -> Value& = default;
    Value(Value&&) = default;
    auto operator=(Value&&) -> Value& = default;

    /**
     * Constructs a value with a given typed value.
     * @param value
     */
    template <typename value_type, typename = std::enable_if<is_valid_value_type<value_type>>>
    Value(value_type const& value) : m_value{value} {};

    /**
     * Constructs a string value with a string view.
     * @param str_view
     */
    Value(std::string_view str_view) : m_value{std::move(value_str_t(str_view))} {};

    /**
     * Moving construct a string value.
     * @param str
     */
    Value(std::string&& str) : m_value{std::move(str)} {};

    /**
     * Moving constructs a clp string (four byte encoding).
     * @tparam encoded_variable_t
     * @param clp_str
     */
    template <typename encoded_variable_t>
    Value(ClpString<encoded_variable_t>&& clp_str) : m_value{std::move(clp_str)} {};

    /**
     * @return if the underlying value is null.
     */
    [[nodiscard]] auto is_null() const -> bool {
        return std::holds_alternative<std::monostate>(m_value);
    }

    /**
     * @return if the underlying value is a CLP string.
     */
    [[nodiscard]] auto is_clp_str() const -> bool {
        return std::holds_alternative<value_eight_byte_clp_str_t>(m_value)
               || std::holds_alternative<value_four_byte_clp_str_t>(m_value);
    }

    /**
     * @tparam value_type
     * @return Wether the underlying value is an instance of the given value_type.
     */
    template <typename value_type, typename = std::enable_if<is_valid_value_type<value_type>>>
    [[nodiscard]] auto is_type() const -> bool {
        return std::holds_alternative<value_type>(m_value);
    }

    /**
     * Gets an immutable representation of the underlying value as the given value_type.
     * @tparam value_type
     * @return The underlying value.
     */
    template <typename value_type, typename = std::enable_if<is_valid_value_type<value_type>>>
    [[nodiscard]] auto get() const -> ImmutableReturnType<value_type> {
        if (false == is_type<value_type>()) {
            throw ValueException(ErrorCodeFailure, __FILE__, __LINE__, "Invalid Type Convert");
        }
        return std::get<value_type>(m_value);
    }

    [[nodiscard]] auto dump() const -> std::string {
        if (is_null()) {
            return "null";
        }
        if (is_type<value_four_byte_clp_str_t>()) {
            std::string decoded_clp_str;
            if (false == get<value_four_byte_clp_str_t>().decode(decoded_clp_str)) {
                throw ValueException(
                        ErrorCodeFailure,
                        __FILE__,
                        __LINE__,
                        "Failed to decode CLP string."
                );
            }
            return decoded_clp_str;
        } else if (is_type<value_eight_byte_clp_str_t>()) {
            std::string decoded_clp_str;
            if (false == get<value_eight_byte_clp_str_t>().decode(decoded_clp_str)) {
                throw ValueException(
                        ErrorCodeFailure,
                        __FILE__,
                        __LINE__,
                        "Failed to decode CLP string."
                );
            }
            return decoded_clp_str;
        } else if (is_type<value_int_t>()) {
            return std::to_string(get<value_int_t>());
        } else if (is_type<value_float_t>()) {
            return nlohmann::json(get<value_float_t>()).dump();
        } else if (is_type<value_bool_t>()) {
            return get<value_bool_t>() ? "true" : "false";
        } else if (is_type<value_str_t>()) {
            return std::string{get<value_str_t>()};
        }
        throw ValueException(ErrorCodeFailure, __FILE__, __LINE__, "Invalid Type.");
    }

private:
    value_t m_value{std::monostate{}};
};
};  // namespace clp_s::ffi::ir_stream

#endif
