#ifndef FFI_IR_STREAM_ATTRIBUTES_HPP
#define FFI_IR_STREAM_ATTRIBUTES_HPP

#include <optional>
#include <string>
#include <string_view>
#include <variant>
#include <vector>

#include <json/single_include/nlohmann/json.hpp>

namespace ffi::ir_stream {
using attr_str_t = std::string;
using attr_int_t = int64_t;

template <typename T>
constexpr bool is_valid_attribute_type{std::disjunction_v<
        std::is_same<T, attr_str_t>,
        std::is_same<T, attr_int_t>>};

class AttributeInfo {
public:
    static constexpr char cNameKey[]{"name"};
    static constexpr char cTypeTagKey[]{"type"};

    enum TypeTag : uint8_t {
        String,
        Int,
    };

    AttributeInfo(std::string name, TypeTag type) : m_name{std::move(name)}, m_type{type} {};

    [[nodiscard]] auto get_name() const -> std::string_view { return m_name; }

    [[nodiscard]] auto get_type_tag() const -> TypeTag { return m_type; }

private:
    std::string m_name;
    TypeTag m_type;
};

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

    [[nodiscard]] auto validate_type(AttributeInfo const& attr_info) -> bool {
        auto const type_tag{attr_info.get_type_tag()};
        if (AttributeInfo::TypeTag::String == type_tag) {
            return is_type<attr_str_t>();
        }
        if (AttributeInfo::TypeTag::Int == type_tag) {
            return is_type<attr_int_t>();
        }
        return false;
    }

private:
    std::variant<attr_str_t, attr_int_t> m_attribute;
};

auto to_json(nlohmann::json& data, AttributeInfo const& attr_info) -> void;
}  // namespace ffi::ir_stream

#endif
