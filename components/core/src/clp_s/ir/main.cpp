#include <chrono>
#include <fstream>
#include <iostream>
#include <msgpack.hpp>

#include <Catch2/single_include/catch2/catch.hpp>
#include <json/single_include/nlohmann/json.hpp>

#include "../../clp/streaming_compression/zstd/Compressor.hpp"
#include "../../clp/BufferReader.hpp"
#include "../../clp/FileReader.hpp"
#include "../ffi/ir_stream/deserialization_methods.hpp"
#include "../ffi/ir_stream/serialization_methods.hpp"
#include "../ffi/ir_stream/SerializationBuffer.hpp"

using clp_s::ffi::ir_stream::deserialize_next_key_value_pair_record;
using clp_s::ffi::ir_stream::deserialize_record_as_json_str;
using clp_s::ffi::ir_stream::IRErrorCode;
using clp_s::ffi::ir_stream::SerializationBuffer;
using clp_s::ffi::ir_stream::serialize_end_of_stream;
using clp_s::ffi::ir_stream::serialize_key_value_pair_record;
using clp_s::ffi::ir_stream::Value;
using clp_s::ffi::SchemaTree;
using clp_s::ffi::SchemaTreeNode;
using clp::streaming_compression::zstd::Compressor;

namespace {
class Timer {
public:
    using TimePoint = decltype(std::chrono::high_resolution_clock::now());

    Timer() : m_start{std::chrono::high_resolution_clock::now()} {}

    [[nodiscard]] auto get_time_used_in_microsecond() const -> long long {
        auto const end{std::chrono::high_resolution_clock::now()};
        auto const elapsed{std::chrono::duration_cast<std::chrono::microseconds>(end - m_start)};
        return elapsed.count();
    }

private:
    TimePoint m_start;
};

size_t level_map[]{
        1 * 1024 * 1024,
        10 * 1024 * 1024,
        100 * 1024 * 1024,
        1024 * 1024 * 1024,
        10L * 1024 * 1024 * 1024};

auto eva_main(int argc, char const* argv[]) -> int {
    if (1 >= argc) {
        std::cerr << "Error: Incorrect Args.\n";
    }
    std::string_view input_path{argv[1]};

    size_t level{0};
    size_t raw_json_bytes{0};
    size_t msgpack_bytes{0};
    size_t ir_bytes{0};
    long long json_to_msgpack_time{0};
    long long msgpack_to_map_time{0};
    long long map_to_ir_time{0};
    long long ir_deserialize_time{0};

    std::ifstream fin;
    fin.open(std::string(input_path));
    SerializationBuffer buffer;
    std::string line;
    std::vector<SchemaTreeNode::id_t> schema;
    std::vector<std::optional<clp_s::ffi::ir_stream::Value>> values;
    SchemaTree deserialized_schema_tree;
    size_t idx{0};
    std::optional<size_t> last_reported_level{std::nullopt};

    auto result_printer = [&]() -> void {
        nlohmann::json result;
        result.emplace("path", input_path);
        result.emplace("level", level + 1);
        result.emplace("num_lines", idx);
        result.emplace("size_json", raw_json_bytes);
        result.emplace("size_msgpack", msgpack_bytes);
        result.emplace("size_ir", ir_bytes);
        result.emplace(
                "time_json_to_msgpack",
                static_cast<double>(json_to_msgpack_time) / 1000000.0
        );
        result.emplace("time_msgpack_to_map", static_cast<double>(msgpack_to_map_time) / 1000000.0);
        result.emplace("time_map_to_ir", static_cast<double>(map_to_ir_time) / 1000000.0);
        result.emplace(
                "time_ir_deserialization",
                static_cast<double>(ir_deserialize_time) / 1000000.0
        );
        std::cerr << result.dump() << "\n";
        last_reported_level = level;
    };

    while (getline(fin, line)) {
        ++idx;
        raw_json_bytes += (line.size() + 1);  // New line character
        Timer json_to_msgpack_timer;
        nlohmann::json item = nlohmann::json::parse(line);
        auto const msgpack_data{nlohmann::json::to_msgpack(item)};
        json_to_msgpack_time += json_to_msgpack_timer.get_time_used_in_microsecond();

        msgpack_bytes += msgpack_data.size();
        Timer msgpack_to_map_timer;
        msgpack::object_handle oh;
        msgpack::unpack(
                oh,
                reinterpret_cast<char const*>(msgpack_data.data()),
                msgpack_data.size()
        );
        msgpack_to_map_time += msgpack_to_map_timer.get_time_used_in_microsecond();

        Timer map_to_ir_timer;
        if (false == serialize_key_value_pair_record(oh.get(), buffer)) {
            std::cerr << "Failed to serialize: (#" << idx << ")" << line << "\n";
            return -1;
        }
        map_to_ir_time += map_to_ir_timer.get_time_used_in_microsecond();
        auto const ir_buf{buffer.get_ir_buf()};
        ir_bytes += ir_buf.size();

        clp::BufferReader buffer_reader(ir_buf.data(), ir_buf.size());
        Timer ir_deserialize_timer;
        if (IRErrorCode::Success
            != deserialize_next_key_value_pair_record(
                    buffer_reader,
                    deserialized_schema_tree,
                    schema,
                    values
            ))
        {
            std::cerr << "Failed to deserialize (#" << idx << "): " << line << "\n";
            return -1;
        }
        ir_deserialize_time += ir_deserialize_timer.get_time_used_in_microsecond();
        buffer.flush_ir_buf();

        if (raw_json_bytes > level_map[level]) {
            result_printer();
            if (level == 4) {
                break;
            }
            ++level;
        }
    }
    if (false == last_reported_level.has_value() || last_reported_level.value() != level) {
        result_printer();
    }
    if (buffer.get_schema_tree().get_size() != deserialized_schema_tree.get_size()) {
        std::cerr << "The deserialized tree's size doesn't match.\n";
    }
    return 0;
}
}  // namespace

auto main(int argc, char const* argv[]) -> int {
//    if (2 >= argc) {
//        std::cerr << "Error: Incorrect Args.\n";
//    }
//    std::string_view input_path{argv[1]};
//    std::string_view output_path{argv[2]};
//
//    std::ifstream fin;
//    fin.open(std::string(input_path));
//    SerializationBuffer buffer;
//    std::string line;
//    size_t idx{0};
//    long long map_to_ir_time{0};
//    clp::FileWriter writer;
//    Compressor zstd_compressor;
//
//    writer.open(std::string{output_path}, clp::FileWriter::OpenMode::CREATE_FOR_WRITING);
//    zstd_compressor.open(writer);
//
//    while (getline(fin, line)) {
//        ++idx;
//        nlohmann::json item = nlohmann::json::parse(line);
//        auto const msgpack_data{nlohmann::json::to_msgpack(item)};
//
//        msgpack::object_handle oh;
//        msgpack::unpack(
//                oh,
//                reinterpret_cast<char const*>(msgpack_data.data()),
//                msgpack_data.size()
//        );
//
//        Timer map_to_ir_timer;
//        if (false == serialize_key_value_pair_record(oh.get(), buffer)) {
//            std::cerr << "Failed to serialize: (#" << idx << ")" << line << "\n";
//            return -1;
//        }
//        auto const ir_buf{buffer.get_ir_buf()};
//        zstd_compressor.write(ir_buf.data(), ir_buf.size());
//        buffer.flush_ir_buf();
//        map_to_ir_time += map_to_ir_timer.get_time_used_in_microsecond();
//    }
//    std::cerr << "Compression time: " << static_cast<double>(map_to_ir_time) / 1000000.0 << "\n";
//    zstd_compressor.write("\0", 1);
//    zstd_compressor.close();
//    writer.close();
//
//    return 0;

    if (2 != argc) {
        std::cerr << "Error: Incorrect Args.\n";
    }
    std::string_view input_path{argv[1]};
    clp::FileReader reader;
    reader.open(std::string{input_path});
    SchemaTree schema_tree;
    std::vector<SchemaTreeNode::id_t> schema;
    std::vector<std::optional<clp_s::ffi::ir_stream::Value>> values;
    std::string json_str;

    std::string line;
    size_t idx{0};
    while (true) {
        auto const err{deserialize_next_key_value_pair_record(reader, schema_tree, schema, values)};
        if (IRErrorCode::EndOfStream == err) {
            break;
        } else if (IRErrorCode::Success != err) {
            std::cerr << "Failed... Idx: " << idx << "\n";
            break;
        }

        if (false == deserialize_record_as_json_str(schema_tree, schema, values, json_str)) {
            std::cerr << "Failed to deserialize to json...\n";
        }

        std::cout << json_str << std::endl;

        ++idx;
    }
    return 0;
}
