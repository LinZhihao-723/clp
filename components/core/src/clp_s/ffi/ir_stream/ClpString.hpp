#ifndef CLP_S_FFI_IR_STREAM_CLP_STRING_HPP
#define CLP_S_FFI_IR_STREAM_CLP_STRING_HPP

#include <string>
#include <vector>

#include "../../../clp/ffi/ir_stream/decoding_methods.hpp"
#include "../../../clp/ir/types.hpp"

namespace clp_s::ffi::ir_stream {
using four_byte_encoded_variable_t = clp::ir::four_byte_encoded_variable_t;
using eight_byte_encoded_variable_t = clp::ir::eight_byte_encoded_variable_t;

template <typename encoded_variable_t>
class ClpString {
public:
    ClpString(
            std::string logtype,
            std::vector<std::string> dict_vars,
            std::vector<encoded_variable_t> encoded_vars
    )
            : m_logtype{std::move(logtype)},
              m_dict_vars{std::move(dict_vars)},
              m_encoded_vars{std::move(encoded_vars)} {}

    ClpString() = default;
    ClpString(ClpString const&) = default;
    ClpString(ClpString&&) = default;
    auto operator=(ClpString const&) -> ClpString& = default;
    auto operator=(ClpString&&) -> ClpString& = default;

    [[nodiscard]] auto get_logtype() const -> std::string const& { return m_logtype; }

    [[nodiscard]] auto get_dict_vars() const -> std::vector<std::string> const& {
        return m_dict_vars;
    }

    [[nodiscard]] auto get_encoded_vars() const -> std::vector<encoded_variable_t> const& {
        return m_encoded_vars;
    }

    [[nodiscard]] auto get_logtype() -> std::string& { return m_logtype; }

    [[nodiscard]] auto get_dict_vars() -> std::vector<std::string>& { return m_dict_vars; }

    [[nodiscard]] auto get_encoded_vars() -> std::vector<encoded_variable_t>& {
        return m_encoded_vars;
    }

    /**
     * Decodes the CLP string.
     * @param str Outputs the decoded string.
     * @return true on success, false if the decoding failed.
     */
    [[nodiscard]] auto decode(std::string& str) const -> bool {
        str.clear();
        if constexpr (std::is_same_v<encoded_variable_t, four_byte_encoded_variable_t>) {
            auto const err{clp::ffi::ir_stream::four_byte_encoding::decode_clp_str(
                    m_logtype,
                    m_encoded_vars,
                    m_dict_vars,
                    str
            )};
            if (clp::ffi::ir_stream::IRErrorCode::IRErrorCode_Success != err) {
                return false;
            }
        } else {
            auto const err{clp::ffi::ir_stream::eight_byte_encoding::decode_clp_str(
                    m_logtype,
                    m_encoded_vars,
                    m_dict_vars,
                    str
            )};
            if (clp::ffi::ir_stream::IRErrorCode::IRErrorCode_Success != err) {
                return false;
            }
        }
        return true;
    }

private:
    std::string m_logtype;
    std::vector<std::string> m_dict_vars;
    std::vector<encoded_variable_t> m_encoded_vars;
};

using FourByteEncodingClpString = ClpString<four_byte_encoded_variable_t>;
using EightByteEncodingClpString = ClpString<eight_byte_encoded_variable_t>;
}  // namespace clp_s::ffi::ir_stream

#endif
