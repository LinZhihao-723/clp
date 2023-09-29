#ifndef FFI_IR_STREAM_ATTRIBUTES_HPP
#define FFI_IR_STREAM_ATTRIBUTES_HPP

#include <optional>
#include <string>
#include <variant>

namespace ffi::ir_stream {
class Attribute {
public:
    [[nodiscard]] auto has_value() const -> bool {
        return m_attribute.has_value();
    } 
    [[nodiscard]] auto is_string() const -> bool {
        return m_attribute.has_value() && std::holds_alternative<std::string>(m_attribute.value());
    }
    [[nodiscard]] auto is_int() const -> bool {
        return m_attribute.has_value() && std::holds_alternative<int64_t>(m_attribute.value());
    }
private:
    std::optional<std::variant<std::string, int64_t>> m_attribute;
};
}

#endif