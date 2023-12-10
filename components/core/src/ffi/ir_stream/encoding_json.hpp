#ifndef FFI_IR_ENCODING_JSON
#define FFI_IR_ENCODING_JSON

#include <vector>

#include <json/single_include/nlohmann/json.hpp>

#include "SchemaTree.hpp"

namespace ffi::ir_stream {
auto encode_json_object(
        nlohmann::json const& json,
        SchemaTree& schema_tree,
        std::vector<int8_t>& ir_buf
) -> bool;
};  // namespace ffi::ir_stream

#endif
