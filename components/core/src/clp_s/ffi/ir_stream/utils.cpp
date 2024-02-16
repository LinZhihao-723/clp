#include "utils.hpp"

#include <string_view>

#include <GSL/include/gsl/span>
#include <json/single_include/nlohmann/json.hpp>

namespace clp_s::ffi::ir_stream {
namespace {
/**
 * Escapes a string and append it to the json string.
 * @param str
 * @param json_str
 */
auto escape_and_append_string_to_json_str(std::string_view str, std::string& json_str) -> void {
    // TODO: implement our own function to generate escaped string.
    json_str += nlohmann::json(str).dump();
}

/**
 * Append a msgpack object to the json str. The object can be a primitive value.
 * @param obj
 * @param json_str
 * @return true on success, false on failure.
 */
[[nodiscard]] auto append_msgpack_obj_to_json_str(msgpack::object const& obj, std::string& json_str)
        -> bool {
    bool retval{true};
    switch (obj.type) {
        case msgpack::type::MAP:
            retval = append_msgpack_map_to_json_str(obj, json_str);
            break;
        case msgpack::type::ARRAY:
            retval = append_msgpack_array_to_json_str(obj, json_str);
            break;
        case msgpack::type::NIL:
            json_str += "null";
            break;
        case msgpack::type::BOOLEAN:
            json_str += obj.as<bool>() ? "true" : "false";
            break;
        case msgpack::type::STR:
            escape_and_append_string_to_json_str(obj.as<std::string_view>(), json_str);
            break;
        case msgpack::type::FLOAT32:
        case msgpack::type::FLOAT:
            json_str += nlohmann::json(obj.as<double>()).dump();
            break;
        case msgpack::type::POSITIVE_INTEGER:
            json_str += std::to_string(obj.as<uint64_t>());
            break;
        case msgpack::type::NEGATIVE_INTEGER:
            json_str += std::to_string(obj.as<int64_t>());
            break;
        default:
            retval = false;
            break;
    }
    return retval;
}
}  // namespace

auto append_msgpack_array_to_json_str(msgpack::object const& array, std::string& json_str) -> bool {
    json_str.push_back('[');
    auto const array_data{array.via.array};
    bool is_first_element{true};
    for (auto const& element : gsl::span{array_data.ptr, static_cast<size_t>(array_data.size)}) {
        if (is_first_element) {
            is_first_element = false;
        } else {
            json_str.push_back(',');
        }
        if (false == append_msgpack_obj_to_json_str(element, json_str)) {
            return false;
        }
    }
    json_str.push_back(']');
    return true;
}

auto append_msgpack_map_to_json_str(msgpack::object const& map, std::string& json_str) -> bool {
    json_str.push_back('{');
    auto const& map_data{map.via.map};
    bool is_first_element{true};
    for (auto const& [key, val] : gsl::span{map_data.ptr, static_cast<size_t>(map_data.size)}) {
        if (is_first_element) {
            is_first_element = false;
        } else {
            json_str.push_back(',');
        }
        escape_and_append_string_to_json_str(key.as<std::string_view>(), json_str);
        json_str.push_back(':');
        if (false == append_msgpack_obj_to_json_str(val, json_str)) {
            return false;
        }
    }
    json_str.push_back('}');
    return true;
}
}  // namespace clp_s::ffi::ir_stream
