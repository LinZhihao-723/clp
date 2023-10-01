#ifndef FFI_IR_STREAM_ATTRIBUTES_HPP
#define FFI_IR_STREAM_ATTRIBUTES_HPP

#include <optional>
#include <string>
#include <string_view>
#include <variant>
#include <vector>

namespace ffi::ir_stream {
using attr_str_t = std::string;
using attr_int_t = int64_t;

template <typename T>
constexpr bool is_valid_attribute_type{std::disjunction_v<
        std::is_same<T, attr_str_t>,
        std::is_same<T, attr_int_t>>};

class Attribute {
public:
    template <typename attr_type, typename = std::enable_if<is_valid_attribute_type<attr_type>>>
    Attribute(attr_type const& value) : m_attribute{value} {};
    ~Attribute() = default;

    template <typename attr_type, typename = std::enable_if<is_valid_attribute_type<attr_type>>>
    [[nodiscard]] auto is_type() const -> bool {
        return std::holds_alternative<attr_type>(m_attribute);
    }

    template <typename attr_type, typename = std::enable_if<is_valid_attribute_type<attr_type>>>
    [[nodiscard]] auto get_value() const -> typename std::
            conditional<std::is_same_v<attr_type, attr_str_t>, std::string_view, attr_int_t>::type {
        return std::get<attr_type>(m_attribute);
    }

    [[nodiscard]] auto encode(std::vector<int8_t>& ir_buf) const -> bool;

    [[nodiscard]] auto operator==(Attribute const& rhs) const -> bool {
        return m_attribute == rhs.m_attribute;
    }

private:
    std::variant<attr_str_t, attr_int_t> m_attribute;
};
}  // namespace ffi::ir_stream

#endif
