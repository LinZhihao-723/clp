#include <clp_s/capi.h>

#include <cstddef>
#include <exception>
#include <filesystem>
#include <memory>
#include <sstream>
#include <string>
#include <string_view>
#include <tuple>
#include <vector>

#include <spdlog/spdlog.h>

#include <clp_s/ArchiveReader.hpp>
#include <clp_s/InputConfig.hpp>
#include <clp_s/JsonParser.hpp>
#include <clp_s/OutputHandlerImpl.hpp>
#include <clp_s/search/ast/ConvertToExists.hpp>
#include <clp_s/search/ast/EmptyExpr.hpp>
#include <clp_s/search/ast/Expression.hpp>
#include <clp_s/search/ast/NarrowTypes.hpp>
#include <clp_s/search/ast/OrOfAndForm.hpp>
#include <clp_s/search/EvaluateRangeIndexFilters.hpp>
#include <clp_s/search/EvaluateTimestampIndex.hpp>
#include <clp_s/search/kql/kql.hpp>
#include <clp_s/search/Output.hpp>
#include <clp_s/search/SchemaMatch.hpp>
#include <clp_s/Utils.hpp>

namespace {
constexpr int cSuccess{0};
constexpr int cFailure{1};

// Compression defaults mirroring the values used by the clp-s executable and unit tests.
constexpr size_t cDefaultTargetEncodedSize{8ULL * 1024 * 1024 * 1024};  // 8 GiB
constexpr size_t cDefaultMaxDocumentSize{512ULL * 1024 * 1024};  // 512 MiB
constexpr size_t cDefaultMinTableSize{1ULL * 1024 * 1024};  // 1 MiB
constexpr int cDefaultCompressionLevel{3};

// Search is always performed case-sensitively across the C API.
constexpr bool cIgnoreCase{false};

/**
 * Compresses the newline-delimited JSON file(s) at `input_path` into multi-file archive(s) under
 * `archives_dir`.
 * @param input_path
 * @param archives_dir
 * @return Whether compression succeeded.
 */
[[nodiscard]] auto compress(std::string_view input_path, std::string_view archives_dir) -> bool {
    std::filesystem::create_directory(std::filesystem::path{archives_dir});

    clp_s::JsonParserOption option{};
    option.input_paths_and_canonical_filenames.emplace_back(
            clp_s::Path{
                    .source{clp_s::InputSource::Filesystem},
                    .path{std::string{input_path}}
            },
            std::string{input_path}
    );
    option.archives_dir = std::string{archives_dir};
    option.target_encoded_size = cDefaultTargetEncodedSize;
    option.max_document_size = cDefaultMaxDocumentSize;
    option.min_table_size = cDefaultMinTableSize;
    option.compression_level = cDefaultCompressionLevel;
    option.single_file_archive = false;

    clp_s::JsonParser parser{option};
    if (false == parser.ingest()) {
        return false;
    }
    std::ignore = parser.store();
    return true;
}

/**
 * Parses the KQL `query` and runs the archive-independent standardization passes (`OrOfAndForm`,
 * `NarrowTypes`, `ConvertToExists`) to produce a prepared expression ready to evaluate against
 * individual archives.
 * @param query
 * @return The prepared expression on success.
 * @return nullptr if the query is empty or fails to parse or standardize.
 */
[[nodiscard]] auto prepare_query(std::string_view query)
        -> std::shared_ptr<clp_s::search::ast::Expression> {
    auto query_stream = std::istringstream{std::string{query}};
    auto expr = clp_s::search::kql::parse_kql_expression(query_stream);
    if (nullptr == expr) {
        return nullptr;
    }
    if (nullptr != std::dynamic_pointer_cast<clp_s::search::ast::EmptyExpr>(expr)) {
        return nullptr;
    }

    clp_s::search::ast::OrOfAndForm standardize_pass;
    expr = standardize_pass.run(expr);
    if (nullptr == expr) {
        return nullptr;
    }

    clp_s::search::ast::NarrowTypes narrow_pass;
    expr = narrow_pass.run(expr);
    if (nullptr == expr) {
        return nullptr;
    }

    clp_s::search::ast::ConvertToExists convert_pass;
    expr = convert_pass.run(expr);
    if (nullptr == expr) {
        return nullptr;
    }
    return expr;
}

/**
 * Searches the single archive at `archive_path` with the prepared expression `expr`, running the
 * per-archive passes (`EvaluateRangeIndexFilters`, `EvaluateTimestampIndex`, `SchemaMatch`,
 * `Output`) and appending each matching record to `results`.
 * @param archive_path
 * @param expr The prepared expression produced by `prepare_query`.
 * @param results Returns the matching records appended to the collection.
 */
auto search_archive(
        std::string_view archive_path,
        std::shared_ptr<clp_s::search::ast::Expression> const& expr,
        std::vector<clp_s::VectorOutputHandler::QueryResult>& results
) -> void {
    auto archive_reader = std::make_shared<clp_s::ArchiveReader>();
    auto const path = clp_s::Path{
            .source{clp_s::InputSource::Filesystem},
            .path{std::string{archive_path}}
    };
    archive_reader->open(path, clp_s::NetworkAuthOption{});

    auto archive_expr = expr->copy();

    clp_s::search::EvaluateRangeIndexFilters metadata_filter_pass{
            archive_reader->get_range_index(),
            false == cIgnoreCase
    };
    archive_expr = metadata_filter_pass.run(archive_expr);
    if (nullptr == archive_expr
        || nullptr != std::dynamic_pointer_cast<clp_s::search::ast::EmptyExpr>(archive_expr))
    {
        archive_reader->close();
        return;
    }

    auto timestamp_dict = archive_reader->get_timestamp_dictionary();
    clp_s::search::EvaluateTimestampIndex timestamp_index_pass{timestamp_dict};
    if (clp_s::EvaluatedValue::False == timestamp_index_pass.run(archive_expr)) {
        archive_reader->close();
        return;
    }

    auto match_pass = std::make_shared<clp_s::search::SchemaMatch>(
            archive_reader->get_schema_tree(),
            archive_reader->get_schema_map()
    );
    archive_expr = match_pass->run(archive_expr);
    if (nullptr == archive_expr
        || nullptr != std::dynamic_pointer_cast<clp_s::search::ast::EmptyExpr>(archive_expr))
    {
        archive_reader->close();
        return;
    }

    auto output_handler = std::make_unique<clp_s::VectorOutputHandler>(results);
    clp_s::search::Output output_pass{
            match_pass,
            archive_expr,
            archive_reader,
            std::move(output_handler),
            cIgnoreCase
    };
    std::ignore = output_pass.filter();
    archive_reader->close();
}

/**
 * Forwards each collected search result to `callback`.
 * @param results
 * @param callback Invoked once per result, or nullptr to discard results.
 * @param user_data Forwarded verbatim to every `callback` invocation.
 */
auto forward_results(
        std::vector<clp_s::VectorOutputHandler::QueryResult> const& results,
        ClpSSearchResultCallback callback,
        void* user_data
) -> void {
    if (nullptr == callback) {
        return;
    }
    for (auto const& result : results) {
        callback(result.message.c_str(), user_data);
    }
}

/**
 * Searches every archive under `archives_dir` with the KQL `query`, forwarding each matching
 * record's JSON serialization to `callback`.
 * @param archives_dir
 * @param query
 * @param callback Invoked once per matching record, or nullptr to discard results.
 * @param user_data Forwarded verbatim to every `callback` invocation.
 * @return Whether the search succeeded.
 */
[[nodiscard]] auto search(
        std::string_view archives_dir,
        std::string_view query,
        ClpSSearchResultCallback callback,
        void* user_data
) -> bool {
    auto const expr = prepare_query(query);
    if (nullptr == expr) {
        return false;
    }

    std::vector<clp_s::VectorOutputHandler::QueryResult> results;
    for (auto const& entry : std::filesystem::directory_iterator{std::filesystem::path{archives_dir}
         })
    {
        search_archive(entry.path().string(), expr, results);
    }

    forward_results(results, callback, user_data);
    return true;
}

/**
 * Searches the single archive at `archive_path` with the KQL `query`, forwarding each matching
 * record's JSON serialization to `callback`.
 * @param archive_path
 * @param query
 * @param callback Invoked once per matching record, or nullptr to discard results.
 * @param user_data Forwarded verbatim to every `callback` invocation.
 * @return Whether the search succeeded.
 */
[[nodiscard]] auto search_single_archive(
        std::string_view archive_path,
        std::string_view query,
        ClpSSearchResultCallback callback,
        void* user_data
) -> bool {
    auto const expr = prepare_query(query);
    if (nullptr == expr) {
        return false;
    }

    std::vector<clp_s::VectorOutputHandler::QueryResult> results;
    search_archive(archive_path, expr, results);

    forward_results(results, callback, user_data);
    return true;
}
}  // namespace

auto clp_s_compress(char const* input_path, char const* archives_dir) -> int {
    if (nullptr == input_path || nullptr == archives_dir) {
        return cFailure;
    }
    try {
        if (compress(input_path, archives_dir)) {
            return cSuccess;
        }
        return cFailure;
    } catch (std::exception const& e) {
        SPDLOG_ERROR("clp_s_compress encountered an error - {}", e.what());
        return cFailure;
    } catch (...) {
        SPDLOG_ERROR("clp_s_compress encountered an unknown error");
        return cFailure;
    }
}

auto clp_s_search(
        char const* archives_dir,
        char const* query,
        ClpSSearchResultCallback callback,
        void* user_data
) -> int {
    if (nullptr == archives_dir || nullptr == query) {
        return cFailure;
    }
    try {
        if (search(archives_dir, query, callback, user_data)) {
            return cSuccess;
        }
        return cFailure;
    } catch (std::exception const& e) {
        SPDLOG_ERROR("clp_s_search encountered an error - {}", e.what());
        return cFailure;
    } catch (...) {
        SPDLOG_ERROR("clp_s_search encountered an unknown error");
        return cFailure;
    }
}

auto clp_s_search_archive(
        char const* archive_path,
        char const* query,
        ClpSSearchResultCallback callback,
        void* user_data
) -> int {
    if (nullptr == archive_path || nullptr == query) {
        return cFailure;
    }
    try {
        if (search_single_archive(archive_path, query, callback, user_data)) {
            return cSuccess;
        }
        return cFailure;
    } catch (std::exception const& e) {
        SPDLOG_ERROR("clp_s_search_archive encountered an error - {}", e.what());
        return cFailure;
    } catch (...) {
        SPDLOG_ERROR("clp_s_search_archive encountered an unknown error");
        return cFailure;
    }
}
