#ifndef CLP_IR_ENCODEDTEXTAST_HPP
#define CLP_IR_ENCODEDTEXTAST_HPP

#include <string>
#include <utility>
#include <vector>

#include "types.hpp"

namespace clp::ir {
/**
 * A parsed and encoded unstructured text string.
 * @tparam encoded_variable_t The type of encoded variables in the string.
 */
template <typename encoded_variable_t>
class EncodedTextAst {
public:
    // Constructor
    explicit EncodedTextAst(
            std::string logtype,
            std::vector<std::string> dict_vars,
            std::vector<encoded_variable_t> encoded_vars
    )
            : m_logtype{std::move(logtype)},
              m_dict_vars{std::move(dict_vars)},
              m_encoded_vars{std::move(encoded_vars)} {}

    // Disable copy constructor and assignment operator
    EncodedTextAst(EncodedTextAst const&) = delete;
    auto operator=(EncodedTextAst const&) -> EncodedTextAst& = delete;

    // Default move constructor and assignment operator
    EncodedTextAst(EncodedTextAst&&) = default;
    auto operator=(EncodedTextAst&&) -> EncodedTextAst& = default;

    // Destructor
    ~EncodedTextAst() = default;

    // Methods
    [[nodiscard]] auto get_logtype() const -> std::string const& { return m_logtype; }

    [[nodiscard]] auto get_dict_vars() const -> std::vector<std::string> const& {
        return m_dict_vars;
    }

    [[nodiscard]] auto get_encoded_vars() const -> std::vector<encoded_variable_t> const& {
        return m_encoded_vars;
    }

private:
    // Variables
    std::string m_logtype;
    std::vector<std::string> m_dict_vars;
    std::vector<encoded_variable_t> m_encoded_vars;
};

using EightByteEncodedTextAst = EncodedTextAst<eight_byte_encoded_variable_t>;
using FourByteEncodedTextAst = EncodedTextAst<four_byte_encoded_variable_t>;
}  // namespace clp::ir

#endif  // CLP_IR_ENCODEDTEXTAST_HPP
