#ifndef FFI_IR_KEY_VALUE_RECORD_HPP
#define FFI_IR_KEY_VALUE_RECORD_HPP

#include <optional>
#include <string>
#include <string_view>
#include <variant>
#include <vector>

#include "../../TraceableException.hpp"
#include "Values.hpp"

namespace ffi::ir_stream {
class KeyValuePair;
class KeyValueRecord;
class KeyValuePairMap;
class KeyValueRecordArray;

enum class KeyValueRecordType : uint8_t {
    KeyValuePairMap = 0,
    KeyValueRecordArray
};

class KeyValuePairException : public TraceableException {
public:
    KeyValuePairException(
            ErrorCode error_code,
            char const* const filename,
            int line_number,
            std::string message
    )
            : TraceableException{error_code, filename, line_number},
              m_message{std::move(message)} {}

    [[nodiscard]] char const* what() const noexcept override { return m_message.c_str(); }

private:
    std::string m_message;
};

class KeyValuePairMap {
public:
    KeyValuePairMap() = default;
    ~KeyValuePairMap() = default;

    [[nodiscard]] auto contains(std::string const& key) const -> bool {
        return (m_map.end() != m_map.find(key));
    }

    [[nodiscard]] auto add_nested_record(std::string const& key, KeyValueRecordType type) -> bool {
        auto const [it, inserted]{m_map.emplace(key, key, type)};
        return inserted;
    }

    template <typename value_type, typename = std::enable_if<is_valid_value_type<value_type>>>
    [[nodiscard]] auto add_pair(std::string const& key, value_t value) -> bool {
        auto const [it, inserted]{m_map.emplace(key, key, value)};
        return inserted;
    }

    [[nodiscard]] auto add_null(std::string const& key) -> bool {
        auto const [it, inserted]{m_map.emplace(key, key)};
        return inserted;
    }

    [[nodiscard]] auto add(KeyValuePair const& key_value_pair) -> bool {
        auto const [it, inserted]{m_map.emplace(key_value_pair.get_key(), key_value_pair)};
        return inserted;
    }

    [[nodiscard]] auto at(std::string const& key) -> KeyValuePair& {
        if (false == contains(key)) {
            throw KeyValuePairException(
                    ErrorCode_OutOfBounds,
                    __FILENAME__,
                    __LINE__,
                    "Key not found"
            );
        }
        return m_map.at(key);
    }

    [[nodiscard]] auto at(std::string const& key) const -> KeyValuePair const& {
        if (false == contains(key)) {
            throw KeyValuePairException(
                    ErrorCode_OutOfBounds,
                    __FILENAME__,
                    __LINE__,
                    "Key not found"
            );
        }
        return m_map.at(key);
    }

    [[nodiscard]] auto operator[](std::string const& key) -> KeyValuePair& { return at(key); }

    [[nodiscard]] auto operator[](std::string const& key) const -> KeyValuePair const& {
        return at(key);
    }

    [[nodiscard]] auto get_map() const -> std::unordered_map<std::string, KeyValuePair> const& {
        return m_map;
    }

    [[nodiscard]] auto get_size() const -> size_t { return m_map.size(); }

private:
    std::unordered_map<std::string, KeyValuePair> m_map;
};

class KeyValueRecordArray {
public:
    KeyValueRecordArray() = default;
    ~KeyValueRecordArray() = default;

    [[nodiscard]] auto get_size() const -> size_t { return m_records.size(); }

    [[nodiscard]] auto get_records() const -> std::vector<KeyValueRecord> const& {
        return m_records;
    }

    auto add_new_record(KeyValueRecordType type) -> void { m_records.emplace_back(type); }

    auto add(KeyValueRecord const& record) -> void { m_records.emplace_back(record); }

    [[nodiscard]] auto at(size_t idx) -> KeyValueRecord& {
        if (get_size() <= idx) {
            throw KeyValuePairException(
                    ErrorCode_OutOfBounds,
                    __FILENAME__,
                    __LINE__,
                    "Idx out of bound."
            );
        }
        return m_records[idx];
    }

    [[nodiscard]] auto at(size_t idx) const -> KeyValueRecord const& {
        if (get_size() <= idx) {
            throw KeyValuePairException(
                    ErrorCode_OutOfBounds,
                    __FILENAME__,
                    __LINE__,
                    "Idx out of bound."
            );
        }
        return m_records[idx];
    }

    [[nodiscard]] auto operator[](size_t idx) -> KeyValueRecord& { return at(idx); }

    [[nodiscard]] auto operator[](size_t idx) const -> KeyValueRecord const& { return at(idx); }

private:
    std::vector<KeyValueRecord> m_records;
};

class KeyValueRecord {
public:
    KeyValueRecord(KeyValueRecordType type) : m_type{type} {
        // if (Type::KeyValuePairMap == type) {
        //     m_record = KeyValuePairMap{};
        // } else {
        //     m_record = KeyValueRecordArray{};
        // }
        m_record = KeyValuePairMap{};
    }

    ~KeyValueRecord() = default;

    [[nodiscard]] auto get_type() const -> KeyValueRecordType { return m_type; }

    // [[nodiscard]] auto get_key_value_pair_map() -> KeyValuePairMap& {
    //     if (Type::KeyValuePairMap != m_type) {
    //         throw KeyValuePairException(
    //                 ErrorCode_Failure,
    //                 __FILENAME__,
    //                 __LINE__,
    //                 "The underlying data type is not KeyValuePairMap"
    //         );
    //     }
    //     return std::get<KeyValuePairMap>(m_record);
    // }

    // [[nodiscard]] auto get_key_value_pair_map() const -> KeyValuePairMap const& {
    //     if (Type::KeyValuePairMap != m_type) {
    //         throw KeyValuePairException(
    //                 ErrorCode_Failure,
    //                 __FILENAME__,
    //                 __LINE__,
    //                 "The underlying data type is not KeyValuePairMap"
    //         );
    //     }
    //     return std::get<KeyValuePairMap>(m_record);
    // }

    // [[nodiscard]] auto get_key_value_record_array() -> KeyValueRecordArray& {
    //     if (Type::KeyValueRecordArray != m_type) {
    //         throw KeyValuePairException(
    //                 ErrorCode_Failure,
    //                 __FILENAME__,
    //                 __LINE__,
    //                 "The underlying data type is not KeyValueRecordArray"
    //         );
    //     }
    //     return std::get<KeyValueRecordArray>(m_record);
    // }

    // [[nodiscard]] auto get_key_value_record_array() const -> KeyValueRecordArray const& {
    //     if (Type::KeyValueRecordArray != m_type) {
    //         throw KeyValuePairException(
    //                 ErrorCode_Failure,
    //                 __FILENAME__,
    //                 __LINE__,
    //                 "The underlying data type is not KeyValueRecordArray"
    //         );
    //     }
    //     return std::get<KeyValueRecordArray>(m_record);
    // }

private:
    // std::variant<KeyValuePairMap, KeyValueRecordArray> m_record;
    KeyValuePairMap m_record;
    KeyValueRecordType m_type;
};

class KeyValuePair {
public:
    KeyValuePair(std::string key) : m_key{std::move(key)} {}

    template <typename value_type, typename = std::enable_if<is_valid_value_type<value_type>>>
    KeyValuePair(std::string key, value_type const& val)
            : m_key{std::move(key)},
              m_val{std::move(Value{val})} {}

    KeyValuePair(std::string key, KeyValueRecord record)
            : m_key{std::move(key)},
              m_val{std::move(record)} {}

    KeyValuePair(std::string key, KeyValueRecordType record_type)
            : m_key{std::move(key)},
              m_val{std::move(KeyValueRecord{record_type})} {}

    [[nodiscard]] auto get_key() const -> std::string_view { return m_key; }

    [[nodiscard]] auto is_null() const -> bool {
        return std::holds_alternative<std::monostate>(m_val);
    }

    [[nodiscard]] auto has_nested_record() const -> bool {
        return std::holds_alternative<KeyValueRecord>(m_val);
    }

    [[nodiscard]] auto get_nested_record() const -> KeyValueRecord const& {
        if (is_null() || false == has_nested_record()) {
            throw KeyValuePairException(
                    ErrorCode_Failure,
                    __FILENAME__,
                    __LINE__,
                    "The key value pair doesn't contain nested record."
            );
        }
        return std::get<KeyValueRecord>(m_val);
    }

    [[nodiscard]] auto get_nested_record() -> KeyValueRecord& {
        if (is_null() || false == has_nested_record()) {
            throw KeyValuePairException(
                    ErrorCode_Failure,
                    __FILENAME__,
                    __LINE__,
                    "The key value pair doesn't contain nested record."
            );
        }
        return std::get<KeyValueRecord>(m_val);
    }

    [[nodiscard]] auto is_value() const -> bool { return std::holds_alternative<Value>(m_val); }

    [[nodiscard]] auto get_value() const -> Value const& {
        if (is_null() || false == is_value()) {
            throw KeyValuePairException(
                    ErrorCode_Failure,
                    __FILENAME__,
                    __LINE__,
                    "The key value pair doesn't contain a base-type value."
            );
        }
        return std::get<Value>(m_val);
    }

private:
    std::string m_key;
    std::variant<std::monostate, Value, KeyValueRecord> m_val{std::monostate{}};
};
}  // namespace ffi::ir_stream

#endif
