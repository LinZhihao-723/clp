// Catch2
#include <Catch2/single_include/catch2/catch.hpp>

#include "../src/ffi/ir_stream/SchemaTree.hpp"

using ffi::ir_stream::SchemaTree;
using ffi::ir_stream::SchemaTreeException;
using ffi::ir_stream::SchemaTreeNode;
using ffi::ir_stream::SchemaTreeNodeValueType;

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

    test_node(SchemaTree::cRootId, "a", SchemaTreeNodeValueType::Obj, 1, true);
    test_node(SchemaTree::cRootId, "a", SchemaTreeNodeValueType::Int, 2, true);
    test_node(1, "b", SchemaTreeNodeValueType::Obj, 3, true);
    test_node(3, "c", SchemaTreeNodeValueType::Obj, 4, true);

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
