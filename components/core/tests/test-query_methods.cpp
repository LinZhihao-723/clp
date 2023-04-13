// C++ standard libraries
#include <unordered_map>

// Catch2
#include "../submodules/Catch2/single_include/catch2/catch.hpp"

// Project headers
#include "../src/ffi/encoding_methods.hpp"
#include "../src/ffi/search/ExactVariableToken.hpp"
#include "../src/ffi/search/query_methods.hpp"
#include "../src/ffi/search/QueryMethodFailed.hpp"
#include "../src/ffi/search/WildcardToken.hpp"

using ffi::eight_byte_encoded_variable_t;
using ffi::four_byte_encoded_variable_t;
using ffi::search::ExactVariableToken;
using ffi::search::generate_subqueries;
using ffi::search::TokenType;
using ffi::search::WildcardToken;
using ffi::VariablePlaceholder;
using std::string;
using std::variant;
using std::vector;

/**
 * Simple class to hold the type of a query variable
 */
struct QueryVariableType {
public:
    QueryVariableType () = default;
    QueryVariableType (bool is_exact, VariablePlaceholder interpretation) :
            is_exact(is_exact), interpretation(interpretation) {}

    bool is_exact;
    VariablePlaceholder interpretation;
};

TEMPLATE_TEST_CASE("ffi::search::query_methods", "[ffi][search][query_methods]",
                   eight_byte_encoded_variable_t, four_byte_encoded_variable_t)
{
    using TestTypeExactVariableToken = ExactVariableToken<TestType>;
    using TestTypeWildcardVariableToken = WildcardToken<TestType>;

    string wildcard_query;
    vector<std::pair<string, vector<variant<TestTypeExactVariableToken,
            TestTypeWildcardVariableToken>>>> subqueries;

    SECTION("Empty query") {
        REQUIRE_THROWS_AS(generate_subqueries(wildcard_query, subqueries),
                          ffi::search::QueryMethodFailed);
    }

    SECTION("\"*\"") {
        wildcard_query = "*";
        generate_subqueries(wildcard_query, subqueries);
        REQUIRE(subqueries.size() == 1);
        const auto& subquery = subqueries.front();
        REQUIRE(subquery.first == wildcard_query);
    }

    SECTION("No wildcards") {
        // Encode a message
        string message;
        string logtype;
        vector<TestType> encoded_vars;
        vector<int32_t> dictionary_var_bounds;
        vector<string> var_strs = {"4938", std::to_string(INT32_MAX), std::to_string(INT64_MAX),
                                   "0.1", "-25.519686", "-25.5196868642755", "-00.00",
                                   "bin/python2.7.3", "abc123"};
        size_t var_ix = 0;
        message = "here is a string with a small int " + var_strs[var_ix++];
        message += " and a medium int " + var_strs[var_ix++];
        message += " and a very large int " + var_strs[var_ix++];
        message += " and a small double " + var_strs[var_ix++];
        message += " and a medium double " + var_strs[var_ix++];
        message += " and a weird double " + var_strs[var_ix++];
        message += " and a string with numbers " + var_strs[var_ix++];
        message += " and another string with numbers " + var_strs[var_ix++];
        REQUIRE(ffi::encode_message(message, logtype, encoded_vars, dictionary_var_bounds));

        wildcard_query = message;
        generate_subqueries(wildcard_query, subqueries);
        REQUIRE(subqueries.size() == 1);
        const auto& subquery = subqueries.front();

        // Validate that the subquery matches the encoded message
        REQUIRE(logtype == subquery.first);
        const auto& query_vars = subquery.second;
        size_t dict_var_idx = 0;
        size_t encoded_var_idx = 0;
        for (const auto& query_var : query_vars) {
            REQUIRE(std::holds_alternative<TestTypeExactVariableToken>(query_var));
            const auto& exact_var = std::get<TestTypeExactVariableToken>(query_var);
            if (VariablePlaceholder::Dictionary == exact_var.get_placeholder()) {
                auto begin_pos = dictionary_var_bounds[dict_var_idx];
                auto end_pos = dictionary_var_bounds[dict_var_idx + 1];
                REQUIRE(exact_var.get_value() == message.substr(begin_pos, end_pos - begin_pos));
                dict_var_idx += 2;
            } else {
                REQUIRE(exact_var.get_encoded_value() == encoded_vars[encoded_var_idx]);
                ++encoded_var_idx;
            }
        }
    }

    // This test is meant to encompass most cases without being impossible to
    // write by hand. The cases are organized below in the order that they
    // would be generated by following the process described in the README.
    SECTION("\"*abc*123?456?\"") {
        std::unordered_map<string, vector<QueryVariableType>> logtype_query_to_variable_types;
        const auto map_end = logtype_query_to_variable_types.cend();
        string expected_logtype_query;
        vector<QueryVariableType> variable_types;

        // In the comments below, we use:
        // - \i to denote VariablePlaceholder::Integer,
        // - \f to denote VariablePlaceholder::Float, and
        // - \d to VariablePlaceholder::Dictionary

        // All wildcards treated as delimiters, "*abc*" as static text
        // Expected logtype: "*abc*\i?\i?"
        expected_logtype_query = "*abc*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        expected_logtype_query += '?';
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        variable_types.emplace_back(true, VariablePlaceholder::Integer);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*abc*\f?\i?"
        expected_logtype_query = "*abc*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        expected_logtype_query += '?';
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        variable_types.emplace_back(true, VariablePlaceholder::Integer);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*abc*\d?\i?"
        expected_logtype_query = "*abc*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        expected_logtype_query += '?';
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(true, VariablePlaceholder::Integer);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // All wildcards treated as delimiters, "*abc*" as a dictionary variable
        // Expected logtype: "*\d*\i?\i?"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '*';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        expected_logtype_query += '?';
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        variable_types.emplace_back(true, VariablePlaceholder::Integer);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*\d*\f?\i?"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '*';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        expected_logtype_query += '?';
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        variable_types.emplace_back(true, VariablePlaceholder::Integer);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*\d*\d?\i?"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '*';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        expected_logtype_query += '?';
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(true, VariablePlaceholder::Integer);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Second '*' treated as a non-delimiter
        // Expected logtype: "*\d?\i?"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        expected_logtype_query += '?';
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(true, VariablePlaceholder::Integer);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Second '*' treated as delim, first '?' as non-delim, "*abc*" as
        // static text
        // Expected logtype: "*abc*\i?"
        expected_logtype_query = "*abc*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        expected_logtype_query += '?';
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*abc*\f?"
        expected_logtype_query = "*abc*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        expected_logtype_query += '?';
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*abc*\d?"
        expected_logtype_query = "*abc*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '?';
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Second '*' treated as delim, first '?' as non-delim, "*abc*" as a
        // dictionary variable
        // Expected logtype: "*\d*\i?"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '*';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        expected_logtype_query += '?';
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*\d*\f?"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '*';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        expected_logtype_query += '?';
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*\d*\d?"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '*';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '?';
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Second '*' as non-delim, first '?' as non-delim
        // Expected logtype: "*\d?"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '?';
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Second '*' as delim, first '?' as delim, second '?' as non-delim,
        // "*abc*" as static text
        // Expected logtype: "*abc*\i?\i"
        expected_logtype_query = "*abc*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*abc*\f?\i"
        expected_logtype_query = "*abc*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*abc*\d?\i"
        expected_logtype_query = "*abc*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*abc*\i?\f"
        expected_logtype_query = "*abc*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*abc*\f?\f"
        expected_logtype_query = "*abc*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*abc*\d?\f"
        expected_logtype_query = "*abc*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*abc*\i?\d"
        expected_logtype_query = "*abc*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*abc*\f?\d"
        expected_logtype_query = "*abc*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*abc*\d?\d"
        expected_logtype_query = "*abc*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Second '*' as delim, first '?' as delim, second '?' as non-delim,
        // "*abc*" as a dictionary variable
        // Expected logtype: "*\d*\i?\i"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '*';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*\d*\f?\i"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '*';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*\d*\d?\i"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '*';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*\d*\i?\f"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '*';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*\d*\f?\f"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '*';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*\d*\d?\f"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '*';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*\d*\i?\d"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '*';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*\d*\f?\d"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '*';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*\d*\d?\d"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '*';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Second '*' as non-delim, first '?' as delim, second '?' as non-delim
        // Expected logtype: "*\d?\i"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*\d?\f"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*\d?\d"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '?';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Second '*' as delim, first '?' as non-delim, second '?' as non-delim,
        // "*abc*" as static text
        // Expected logtype: "*abc*\i"
        expected_logtype_query = "*abc*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*abc*\f"
        expected_logtype_query = "*abc*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*abc*\d"
        expected_logtype_query = "*abc*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Second '*' as delim, first '?' as non-delim, second '?' as non-delim,
        // "*abc*" as a dictionary variable
        // Expected logtype: "*\d*\i"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '*';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Integer);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Integer);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*\d*\f"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '*';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Float);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Float);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Expected logtype: "*\d*\d"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        expected_logtype_query += '*';
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        // Second '*' as non-delim, first '?' as non-delim, second '?' as
        // non-delim"*abc*" as a dictionary variable
        // Expected logtype: "*\d"
        expected_logtype_query = "*";
        expected_logtype_query += enum_to_underlying_type(VariablePlaceholder::Dictionary);
        variable_types.clear();
        variable_types.emplace_back(false, VariablePlaceholder::Dictionary);
        logtype_query_to_variable_types.emplace(expected_logtype_query, variable_types);

        wildcard_query = "*abc*123?456?";
        generate_subqueries(wildcard_query, subqueries);
        REQUIRE(subqueries.size() == logtype_query_to_variable_types.size());

        for (const auto& subquery : subqueries) {
            const auto& logtype_query = subquery.first;
            const auto& query_vars = subquery.second;

            auto logtype_query_and_var_types = logtype_query_to_variable_types.find(logtype_query);
            REQUIRE(map_end != logtype_query_and_var_types);
            const auto& expected_var_types = logtype_query_and_var_types->second;
            REQUIRE(expected_var_types.size() == query_vars.size());
            for (size_t i = 0; i < expected_var_types.size(); ++i) {
                const auto& expected_var_type = expected_var_types[i];
                const auto& query_var = query_vars[i];

                if (expected_var_type.is_exact) {
                    REQUIRE(std::holds_alternative<TestTypeExactVariableToken>(query_var));
                    const auto& exact_var = std::get<TestTypeExactVariableToken>(query_var);
                    REQUIRE(expected_var_type.interpretation == exact_var.get_placeholder());
                } else {
                    REQUIRE(std::holds_alternative<TestTypeWildcardVariableToken>(query_var));
                    const auto& wildcard_var = std::get<TestTypeWildcardVariableToken>(query_var);
                    switch (expected_var_type.interpretation) {
                        case VariablePlaceholder::Integer:
                            REQUIRE(TokenType::IntegerVariable
                                    == wildcard_var.get_current_interpretation());
                            break;
                        case VariablePlaceholder::Float:
                            REQUIRE(TokenType::FloatVariable
                                    == wildcard_var.get_current_interpretation());
                            break;
                        case VariablePlaceholder::Dictionary:
                            REQUIRE(TokenType::DictionaryVariable
                                    == wildcard_var.get_current_interpretation());
                            break;
                        default:
                            REQUIRE(false);
                    }
                }
            }
        }
    }
}