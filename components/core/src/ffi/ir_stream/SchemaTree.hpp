#ifndef FFI_IR_SCHEMATREE_HPP
#define FFI_IR_SCHEMATREE_HPP

#include <memory>
#include <string>
#include <string_view>
#include <unordered_map>
#include <vector>

#include "../../TraceableException.hpp"

namespace ffi::ir_stream {
class SchemaTreeException : public TraceableException {
public:
    SchemaTreeException(
            ErrorCode error_code,
            char const* const filename,
            int line_number,
            std::string message
    )
            : TraceableException(error_code, filename, line_number),
              m_message(std::move(message)) {}

    [[nodiscard]] char const* what() const noexcept override { return m_message.c_str(); }

private:
    std::string m_message;
};

enum class SchemaTreeNodeValueType : uint8_t {
    Int = 0,
    Float,
    Bool,
    Str,
    Obj
};

class SchemaTreeNode {
public:
    SchemaTreeNode(
            size_t id,
            size_t parent_id,
            std::string_view key_name,
            SchemaTreeNodeValueType type
    )
            : m_id{id},
              m_parent_id{parent_id},
              m_key_name{key_name.begin(), key_name.end()},
              m_type{type} {}

    ~SchemaTreeNode() = default;

    SchemaTreeNode(SchemaTreeNode const&) = delete;
    auto operator=(SchemaTreeNode const&) -> SchemaTreeNode& = delete;
    SchemaTreeNode(SchemaTreeNode&&) = default;
    auto operator=(SchemaTreeNode&&) -> SchemaTreeNode& = default;

    [[nodiscard]] auto get_id() const -> size_t { return m_id; }

    [[nodiscard]] auto get_parent_id() const -> size_t { return m_parent_id; }

    [[nodiscard]] auto get_key_name() const -> std::string_view { return m_key_name; }

    [[nodiscard]] auto get_children_ids() const -> std::vector<size_t> const& {
        return m_children_ids;
    }

    [[nodiscard]] auto get_type() const -> SchemaTreeNodeValueType { return m_type; }

    auto add_child(size_t child_id) -> void { m_children_ids.push_back(child_id); }

private:
    size_t m_id;
    size_t m_parent_id;
    std::vector<size_t> m_children_ids;
    std::string m_key_name;
    SchemaTreeNodeValueType m_type;
};

class SchemaTree {
public:
    SchemaTree() { m_tree_nodes.emplace_back(cRootId, cRootId, "", SchemaTreeNodeValueType::Obj); }

    ~SchemaTree() = default;

    static constexpr size_t cRootId{0};

    [[nodiscard]] auto get_node_with_id(size_t id) const -> SchemaTreeNode const&;

    /**
     * Checks if a node exists with the given parent id, key name, and type.
     * @param parent_id
     * @param key_name
     * @param type
     * @param node_id This will be set to the target node id if the node exists.
     * @return false if the node doesn't exist.
     * @return true if the node exists, and set `node_id`.
     */
    [[nodiscard]] auto has_node(
            size_t parent_id,
            std::string_view key_name,
            SchemaTreeNodeValueType type,
            size_t& node_id
    ) const -> bool;

    /**
     * Creates a new node with the given parent id, key name, and type.
     * If the node already exists, this function will return false, and set
     * `node_id` to the existing node id.
     * @param parent_id
     * @param key_name
     * @param type
     * @param node_id The id of the target node (either newly inserted or already exists).
     * @return false if the node already exists.
     * @return true if a new node is inserted.
     */
    [[maybe_unused]] auto try_insert_node(
            size_t parent_id,
            std::string_view key_name,
            SchemaTreeNodeValueType type,
            size_t& node_id
    ) -> bool;

private:
    std::vector<SchemaTreeNode> m_tree_nodes;
};
}  // namespace ffi::ir_stream

#endif
