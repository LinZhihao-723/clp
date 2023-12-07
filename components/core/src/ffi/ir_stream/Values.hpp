#ifndef FFI_IR_VALUES_HPP
#define FFI_IR_VALUES_HPP

#include <stdint.h>

#include <string>
#include <tuple>
#include <type_traits>
#include <variant>
#include <vector>

#include "../../ReaderInterface.hpp"
#include "../../TraceableException.hpp"
#include "SchemaTree.hpp"

namespace ffi::ir_stream {
using value_int_t = int64_t;
using value_float_t = double;
using value_bool_t = bool;
using value_str_t = std::string;

/**
 * Template struct that represents a value type identity.
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
        value_type_identity<value_str_t>>;

/**
 * Templates to enum the tuple to create variant.
 */
template <typename value_type, typename...>
struct enum_value_types_to_variant;

template <typename... value_types>
struct enum_value_types_to_variant<std::tuple<value_types...>> {
    using type = std::variant<std::monostate, typename value_types::type...>;
};

/**
 * A super type of all the valid value types.
 */
using value_t = enum_value_types_to_variant<valid_value_types>::type;

/**
 * Templates for value type validations.
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
 * This class wraps the super type for valid values, and provides necessary
 * methods to construct/access a value.
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

        [[nodiscard]] char const* what() const noexcept override { return m_message.c_str(); }

    private:
        std::string m_message;
    };

    /**
     * Constructs a value with a given typed value.
     * @param value
     */
    template <typename value_type, typename = std::enable_if<is_valid_value_type<value_type>>>
    Value(value_type const& value) : m_value{value} {};

    Value() = default;
    ~Value() = default;

    /**
     * @return if the underlying value contains data.
     */
    [[nodiscard]] auto is_empty() const -> bool {
        return std::holds_alternative<std::monostate>(m_value);
    }

    /**
     * @tparam value_type
     * @return Wether the underlying value is an instance of the given
     * value_type.
     */
    template <typename value_type, typename = std::enable_if<is_valid_value_type<value_type>>>
    [[nodiscard]] auto is_type() const -> bool {
        return std::holds_alternative<value_type>(m_value);
    }

    /**
     * Gets an immutable representation of the underlying value as the given
     * value_type.
     * @tparam value_type
     * @return The underlying value.
     */
    template <typename value_type, typename = std::enable_if<is_valid_value_type<value_type>>>
    [[nodiscard]] auto get() const -> typename std::conditional<
            std::is_same_v<value_type, value_str_t>,
            std::string_view,
            value_type>::type {
        return std::get<value_type>(m_value);
    }

    /**
     * Gets an immutable representation of the underlying value if it matches
     * the given type.
     * @tparam value_type
     * @param value output the underlying data if the type matches.
     * @return true if the type matches and the value has been set.
     * @return false otherwise.
     */
    template <typename value_type, typename = std::enable_if<is_valid_value_type<value_type>>>
    [[nodiscard]] auto try_get(typename std::conditional<
                               std::is_same_v<value_type, value_str_t>,
                               std::string_view&,
                               value_type&> value) const -> bool {
        if (is_empty()) {
            return false;
        }
        if (false == is_type<value_type>()) {
            return false;
        }
        value = get<value_type>();
        return true;
    }

    /**
     * Encodes the value into the given IR buffer.
     * @param ir_buf
     * @return true on success.
     * @return false on failure.
     */
    [[nodiscard]] auto encode(std::vector<int8_t>& ir_buf) const -> bool;

    /**
     * @return SchemaTreeNodeValueType based on the underlying value type.
     */
    [[nodiscard]] auto get_expected_schema_tree_node_type() const -> SchemaTreeNodeValueType;

    /**
     * Decodes the next value from the given reader.
     * @param reader
     * @return Decoded value.
     */
    static auto decode(ReaderInterface& reader) -> Value;

private:
    value_t m_value{std::monostate{}};
};
};  // namespace ffi::ir_stream

#endif
