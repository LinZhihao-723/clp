#ifndef CLP_S_FFI_SCHEMA_TREE_NODE_HPP
#define CLP_S_FFI_SCHEMA_TREE_NODE_HPP

#include <cstdint>
#include <string>
#include <vector>

namespace clp_s::ffi {
/**
 * This class represents a node in the schema tree. It tracks the node type, key name, parent id,
 * and all its children.
 */
class SchemaTreeNode {
public:
    using id_t = size_t;

    /**
     * This enum defines all the valid node types.
     */
    enum class Type : uint8_t {
        Int = 0,
        Float,
        Bool,
        Str,
        Array,
        Obj
    };

    SchemaTreeNode(id_t id, id_t parent_id, std::string_view key_name, Type type)
            : m_id{id},
              m_parent_id{parent_id},
              m_key_name{key_name.begin(), key_name.end()},
              m_type{type} {}

    ~SchemaTreeNode() = default;
    SchemaTreeNode(SchemaTreeNode const&) = delete;
    auto operator=(SchemaTreeNode const&) -> SchemaTreeNode& = delete;
    SchemaTreeNode(SchemaTreeNode&&) = default;
    auto operator=(SchemaTreeNode&&) -> SchemaTreeNode& = default;

    [[nodiscard]] auto get_id() const -> id_t { return m_id; }

    [[nodiscard]] auto get_parent_id() const -> id_t { return m_parent_id; }

    [[nodiscard]] auto get_key_name() const -> std::string_view { return m_key_name; }

    [[nodiscard]] auto get_type() const -> Type { return m_type; }

    [[nodiscard]] auto get_children_ids() const -> std::vector<id_t> const& {
        return m_children_ids;
    }

    /**
     * Adds a child node to the end of the children list. Due to the performance concern, it does
     * not check whether the given child already exists.
     * @param child_id The node id of the given child.
     */
    auto add_child(id_t child_id) -> void { m_children_ids.push_back(child_id); }

    /**
     * Removes the last inserted child from the children list.
     */
    auto remove_last_inserted_child() -> void {
        if (m_children_ids.empty()) {
            return;
        }
        m_children_ids.pop_back();
    }

private:
    id_t m_id;
    id_t m_parent_id;
    std::vector<id_t> m_children_ids;
    std::string m_key_name;
    Type m_type;
};
}  // namespace clp_s::ffi
#endif
