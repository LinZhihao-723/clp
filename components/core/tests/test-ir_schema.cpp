// Catch2
#include <iostream>
#include <vector>

#include <Catch2/single_include/catch2/catch.hpp>
#include <json/single_include/nlohmann/json.hpp>

#include "../src/BufferReader.hpp"
#include "../src/ffi/ir_stream/decoding_json.hpp"
#include "../src/ffi/ir_stream/encoding_json.hpp"
#include "../src/ffi/ir_stream/SchemaTree.hpp"
#include "../src/ffi/ir_stream/Values.hpp"

using ffi::ir_stream::IRErrorCode;

using ffi::ir_stream::SchemaTree;
using ffi::ir_stream::SchemaTreeException;
using ffi::ir_stream::SchemaTreeNode;
using ffi::ir_stream::SchemaTreeNodeValueType;

using ffi::ir_stream::decode_json_object;
using ffi::ir_stream::encode_json_object;

using ffi::ir_stream::Value;
using ffi::ir_stream::value_bool_t;
using ffi::ir_stream::value_float_t;
using ffi::ir_stream::value_int_t;
using ffi::ir_stream::value_str_t;

TEST_CASE("schema_tree", "[ffi][schema]") {
    SchemaTree schema_tree;

    // clang-format off
    auto test_node{[&schema_tree](
                size_t parent_id,
                std::string_view key_name,
                SchemaTreeNodeValueType type,
                size_t expected_id,
                bool already_exist
        ) -> void {
            size_t node_id{};
            if (already_exist) {
                REQUIRE(schema_tree.has_node(parent_id, key_name, type, node_id));
                REQUIRE(expected_id == node_id);
                REQUIRE(false == schema_tree.try_insert_node(parent_id, key_name, type, node_id));
            } else {
                REQUIRE(false == schema_tree.has_node(parent_id, key_name, type, node_id));
                REQUIRE(schema_tree.try_insert_node(parent_id, key_name, type, node_id));
            }
            REQUIRE(expected_id == node_id);
        }
    };
    // clang-format on

    test_node(SchemaTree::cRootId, "a", SchemaTreeNodeValueType::Obj, 1, false);
    test_node(SchemaTree::cRootId, "a", SchemaTreeNodeValueType::Int, 2, false);
    test_node(1, "b", SchemaTreeNodeValueType::Obj, 3, false);
    test_node(3, "c", SchemaTreeNodeValueType::Obj, 4, false);
    schema_tree.snapshot();
    test_node(3, "d", SchemaTreeNodeValueType::Int, 5, false);
    test_node(3, "d", SchemaTreeNodeValueType::Bool, 6, false);
    test_node(4, "d", SchemaTreeNodeValueType::Int, 7, false);
    test_node(4, "d", SchemaTreeNodeValueType::Str, 8, false);

    test_node(SchemaTree::cRootId, "a", SchemaTreeNodeValueType::Obj, 1, true);
    test_node(SchemaTree::cRootId, "a", SchemaTreeNodeValueType::Int, 2, true);
    test_node(1, "b", SchemaTreeNodeValueType::Obj, 3, true);
    test_node(3, "c", SchemaTreeNodeValueType::Obj, 4, true);
    test_node(3, "d", SchemaTreeNodeValueType::Int, 5, true);
    test_node(3, "d", SchemaTreeNodeValueType::Bool, 6, true);
    test_node(4, "d", SchemaTreeNodeValueType::Int, 7, true);
    test_node(4, "d", SchemaTreeNodeValueType::Str, 8, true);

    schema_tree.revert();
    test_node(SchemaTree::cRootId, "a", SchemaTreeNodeValueType::Obj, 1, true);
    test_node(SchemaTree::cRootId, "a", SchemaTreeNodeValueType::Int, 2, true);
    test_node(1, "b", SchemaTreeNodeValueType::Obj, 3, true);
    test_node(3, "c", SchemaTreeNodeValueType::Obj, 4, true);
    test_node(3, "d", SchemaTreeNodeValueType::Int, 5, false);
    test_node(3, "d", SchemaTreeNodeValueType::Bool, 6, false);
    test_node(4, "d", SchemaTreeNodeValueType::Int, 7, false);
    test_node(4, "d", SchemaTreeNodeValueType::Str, 8, false);

    bool catch_exception{false};
    try {
        size_t id{};
        auto const not_used{schema_tree.try_insert_node(2, "c", SchemaTreeNodeValueType::Obj, id)};
    } catch (SchemaTreeException const& ex) {
        if (ErrorCode_BadParam == ex.get_error_code()) {
            catch_exception = true;
        }
    }
}

TEST_CASE("values", "[ffi][key_value_pairs]") {
    std::vector<Value> expected_values;
    std::vector<int8_t> ir_buffer;
    std::vector<SchemaTreeNodeValueType> expected_types;

    // Int values
    expected_values.emplace_back(static_cast<value_int_t>(0));
    expected_types.push_back(SchemaTreeNodeValueType::Int);
    expected_values.emplace_back(static_cast<value_int_t>(INT32_MAX));
    expected_types.push_back(SchemaTreeNodeValueType::Int);
    expected_values.emplace_back(static_cast<value_int_t>(INT32_MIN));
    expected_types.push_back(SchemaTreeNodeValueType::Int);
    expected_values.emplace_back(static_cast<value_int_t>(INT32_MAX) + 1);
    expected_types.push_back(SchemaTreeNodeValueType::Int);
    expected_values.emplace_back(static_cast<value_int_t>(INT32_MIN) - 1);
    expected_types.push_back(SchemaTreeNodeValueType::Int);
    expected_values.emplace_back(static_cast<value_int_t>(INT64_MAX));
    expected_types.push_back(SchemaTreeNodeValueType::Int);
    expected_values.emplace_back(static_cast<value_int_t>(INT64_MIN));
    expected_types.push_back(SchemaTreeNodeValueType::Int);

    // Float values
    expected_values.emplace_back(static_cast<value_float_t>(0));
    expected_types.push_back(SchemaTreeNodeValueType::Float);
    expected_values.emplace_back(static_cast<value_float_t>(1.2));
    expected_types.push_back(SchemaTreeNodeValueType::Float);
    expected_values.emplace_back(static_cast<value_float_t>(-1.2));
    expected_types.push_back(SchemaTreeNodeValueType::Float);

    // Bool
    expected_values.emplace_back(true);
    expected_types.push_back(SchemaTreeNodeValueType::Bool);
    expected_values.emplace_back(false);
    expected_types.push_back(SchemaTreeNodeValueType::Bool);

    // Str
    expected_values.emplace_back("");
    expected_types.push_back(SchemaTreeNodeValueType::Str);
    expected_values.emplace_back("This is a test string");
    expected_types.push_back(SchemaTreeNodeValueType::Str);
    std::string short_length;
    for (size_t i{0}; i < static_cast<size_t>(sizeof(uint16_t)); ++i) {
        short_length.append("a");
    }
    expected_values.emplace_back(short_length);
    expected_types.push_back(SchemaTreeNodeValueType::Str);
    std::string int_length;
    for (size_t i{0}; i < static_cast<size_t>(sizeof(uint16_t)); ++i) {
        int_length.append("ab");
    }
    expected_values.emplace_back(int_length);
    expected_types.push_back(SchemaTreeNodeValueType::Str);

    // Null
    expected_values.emplace_back();
    expected_types.push_back(SchemaTreeNodeValueType::Obj);

    // Encode
    for (auto const& value : expected_values) {
        REQUIRE(value.encode(ir_buffer));
    }

    auto const num_values{expected_values.size()};

    // Decode
    BufferReader reader{size_checked_pointer_cast<char const>(ir_buffer.data()), ir_buffer.size()};
    std::vector<Value> decoded_values(num_values);
    for (auto& value : decoded_values) {
        REQUIRE(ffi::ir_stream::IRErrorCode_Success == value.decode_from_reader(reader));
    }

    // Value compare
    for (size_t i{0}; i < num_values; ++i) {
        bool const is_same{expected_values.at(i) == decoded_values.at(i)};
        REQUIRE(is_same);
        REQUIRE(decoded_values.at(i).get_schema_tree_node_type() == expected_types.at(i));
    }
}

TEST_CASE("encoding_method_json_basic", "[ffi][encoding]") {
    // nlohmann::json j = {
    //         {"key1", "value1"},
    //         {"key2", 3.1415926},
    //         {"key4", {{{"key7", "abcd"}}, {{"key8", "efgh"}}, {{{"key9", "a"}, {"key10",
    //         "b"}}}}},
    //         {"key0", {{"key1", {{"key2", {{"key3", false}}}}}}},
    //         {"key3", {{"key4", 0}, {"key5", false}}}};
    nlohmann::json const j1
            = {{"key1", "value1"},
               {"key0", {{"key1", {{"key2", {{"key3", false}}}}}}},
               {"key4", 33},
               {"key5", {{"key6", 77.66}}},
               {"key7", {{"key8", nullptr}}}};
    nlohmann::json const j2
            = {{"key1", 31},
               {"key0", {{"key1", {{"key2", {{"key3", "False"}}}}}}},
               {"key4", 33},
               {"key5", {{"key6", 31.62}}},
               {"key7", nullptr},
               {"key8", {{"key9", "hi"}}}};

    SchemaTree schema_tree;
    std::vector<int8_t> ir_buf;
    std::vector<int8_t> encoded_ir_bytes;

    REQUIRE(encode_json_object(j1, schema_tree, ir_buf));
    encoded_ir_bytes.insert(encoded_ir_bytes.cend(), ir_buf.cbegin(), ir_buf.cend());
    REQUIRE(encode_json_object(j2, schema_tree, ir_buf));
    encoded_ir_bytes.insert(encoded_ir_bytes.cend(), ir_buf.cbegin(), ir_buf.cend());

    SchemaTree decoded_schema_tree;
    nlohmann::json decoded_json_obj;
    BufferReader reader{
            size_checked_pointer_cast<char const>(encoded_ir_bytes.data()),
            encoded_ir_bytes.size()};

    REQUIRE(IRErrorCode::IRErrorCode_Success
            == decode_json_object(reader, decoded_schema_tree, decoded_json_obj));
    REQUIRE(j1 == decoded_json_obj);
    REQUIRE(IRErrorCode::IRErrorCode_Success
            == decode_json_object(reader, decoded_schema_tree, decoded_json_obj));
    REQUIRE(j2 == decoded_json_obj);

    REQUIRE(schema_tree == decoded_schema_tree);
}

TEST_CASE("encoding_method_array_basic", "[ffi][encoding]") {
    nlohmann::json const j
            = {1,
               0.11111,
               false,
               "This is a string",
               nullptr,
               {{"key0", "This is a key value pair record"},
                {"key1", "Key value pair record again, lol"}},
               {1,
                0.11111,
                false,
                "This is a string",
                nullptr,
                {{"key0", "This is a key value pair record"},
                 {"key1", {1, 0.11111, false, nullptr}}}}};

    SchemaTree schema_tree;
    std::vector<int8_t> ir_buf;
    std::vector<int8_t> encoded_ir_bytes;

    REQUIRE(encode_json_object(j, schema_tree, ir_buf));
    encoded_ir_bytes.insert(encoded_ir_bytes.cend(), ir_buf.cbegin(), ir_buf.cend());

    SchemaTree decoded_schema_tree;
    nlohmann::json decoded_json_array;
    BufferReader reader{
            size_checked_pointer_cast<char const>(encoded_ir_bytes.data()),
            encoded_ir_bytes.size()};
    REQUIRE(IRErrorCode::IRErrorCode_Success
            == decode_json_object(reader, decoded_schema_tree, decoded_json_array));
    REQUIRE(decoded_schema_tree == schema_tree);
    REQUIRE(j == decoded_json_array);
}
