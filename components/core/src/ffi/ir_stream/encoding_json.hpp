#ifndef FFI_IR_ENCODING_JSON
#define FFI_IR_ENCODING_JSON

#include <vector>

#include <json/single_include/nlohmann/json.hpp>

#include "SchemaTree.hpp"

namespace ffi::ir_stream {
auto serialize_json_array(
        nlohmann::json const& json_array,
        SchemaTree& schema_tree,
        std::vector<int8_t>& ir_buf,
        std::vector<size_t>& inserted_schema_tree_node_ids
) -> bool;

auto serialize_json_object(
        nlohmann::json const& json,
        SchemaTree& schema_tree,
        std::vector<int8_t>& ir_buf,
        std::vector<size_t>& inserted_schema_tree_node_ids
) -> bool;

auto encode_json_object(
        nlohmann::json const& json,
        SchemaTree& schema_tree,
        std::vector<int8_t>& ir_buf
) -> bool;
};  // namespace ffi::ir_stream

#endif
