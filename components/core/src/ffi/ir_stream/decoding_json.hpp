#ifndef FFI_IR_STREAM_DECODING_JSON_HPP
#define FFI_IR_STREAM_DECODING_JSON_HPP

#include <json/single_include/nlohmann/json.hpp>

#include "../../ReaderInterface.hpp"
#include "decoding_methods.hpp"
#include "SchemaTree.hpp"

namespace ffi::ir_stream {
[[nodiscard]] auto
decode_json_object(ReaderInterface& reader, SchemaTree& schema_tree, nlohmann::json& obj)
        -> IRErrorCode;
}  // namespace ffi::ir_stream

#endif
