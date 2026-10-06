#include "OutputHandlerImpl.hpp"

#include <cerrno>
#include <cstdint>
#include <sstream>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

#include <bsoncxx/builder/basic/document.hpp>
#include <bsoncxx/builder/basic/kvp.hpp>
#include <mongocxx/client.hpp>
#include <mongocxx/collection.hpp>
#include <mongocxx/exception/bulk_write_exception.hpp>
#include <mongocxx/exception/exception.hpp>
#include <mongocxx/instance.hpp>
#include <msgpack.hpp>
#include <spdlog/spdlog.h>

#include <clp/ErrorCode.hpp>
#include <clp_s/ErrorCode.hpp>
#include <clp_s/MongoDBUtils.hpp>
#include <clp_s/ResultsCacheUtils.hpp>

#include "../clp/networking/socket_utils.hpp"
#include "../reducer/CountOperator.hpp"
#include "../reducer/network_utils.hpp"
#include "../reducer/Record.hpp"
#include "archive_constants.hpp"
#include "search/OutputHandler.hpp"
#include "TraceableException.hpp"

using std::string;
using std::string_view;

namespace clp_s {
void FileOutputHandler::write(
        string_view message,
        epochtime_t timestamp,
        string_view archive_id,
        int64_t log_event_idx
) {
    static constexpr string_view cOrigFilePathPlaceholder{""};
    msgpack::type::tuple<epochtime_t, string, string, string, int64_t> const
            src(timestamp, message, cOrigFilePathPlaceholder, archive_id, log_event_idx);
    msgpack::pack(m_file_writer, src);
}

NetworkOutputHandler::NetworkOutputHandler(
        string host,
        int port,
        string session_token,
        uint64_t task_idx
)
        : ::clp_s::search::OutputHandler{true, true},
          m_host{std::move(host)},
          m_port{std::to_string(port)},
          m_session_token{std::move(session_token)},
          m_task_idx{task_idx} {}

auto NetworkOutputHandler::write(
        string_view message,
        epochtime_t timestamp,
        string_view archive_id,
        [[maybe_unused]] int64_t log_event_idx
) -> void {
    constexpr uint32_t cNumHandshakeFields{4};
    constexpr uint32_t cNumResultFields{3};

    msgpack::packer<msgpack::sbuffer> packer{m_buffer};
    if (-1 == m_socket_fd) {
        connect();
        packer.pack_array(cNumHandshakeFields);
        packer.pack_uint8(cProtocolVersion);
        packer.pack(m_session_token);
        packer.pack_uint64(m_task_idx);
        packer.pack(archive_id);
    }
    packer.pack_array(cNumResultFields);
    packer.pack_uint64(m_next_result_idx);
    packer.pack_int64(timestamp);
    packer.pack(message);
    send_buffer();
    ++m_next_result_idx;
}

auto NetworkOutputHandler::write([[maybe_unused]] string_view message) -> void {
    SPDLOG_ERROR("The network output handler requires each result's metadata.");
    throw OperationFailed(ErrorCode::ErrorCodeUnsupported, __FILENAME__, __LINE__);
}

auto NetworkOutputHandler::connect() -> void {
    m_socket_fd = clp::networking::connect_to_server(m_host, m_port);
    if (-1 == m_socket_fd) {
        auto const error{std::error_code{errno, std::generic_category()}};
        SPDLOG_ERROR(
                "Failed to connect to {}:{} - ({}) {}",
                m_host,
                m_port,
                error.value(),
                error.message()
        );
        throw OperationFailed(ErrorCode::ErrorCodeFailureNetwork, __FILENAME__, __LINE__);
    }
}

auto NetworkOutputHandler::send_buffer() -> void {
    if (clp::ErrorCode_Success
        != clp::networking::try_send(m_socket_fd, m_buffer.data(), m_buffer.size()))
    {
        auto const error{std::error_code{errno, std::generic_category()}};
        SPDLOG_ERROR(
                "Failed to send search results to {}:{} - ({}) {}",
                m_host,
                m_port,
                error.value(),
                error.message()
        );
        throw OperationFailed(ErrorCode::ErrorCodeFailureNetwork, __FILENAME__, __LINE__);
    }
    m_buffer.clear();
}

ResultsCacheOutputHandler::ResultsCacheOutputHandler(
        string_view uri,
        string_view collection,
        uint64_t batch_size,
        uint64_t max_num_results,
        string_view dataset,
        bool should_output_timestamp
)
        : ::clp_s::search::OutputHandler{should_output_timestamp, true},
          m_batch_size{batch_size},
          m_max_num_results{max_num_results},
          m_dataset{dataset} {
    m_collection = connect_to_results_cache(uri, collection, m_client);
    m_insert_options.ordered(false);
    m_results.reserve(m_batch_size);
}

ErrorCode ResultsCacheOutputHandler::finish() {
    size_t count = 0;
    while (false == m_latest_results.empty()) {
        auto result = std::move(*m_latest_results.top());
        m_latest_results.pop();

        try {
            m_results.emplace_back(
                    std::move(
                            bsoncxx::builder::basic::make_document(
                                    bsoncxx::builder::basic::kvp(
                                            constants::results_cache::search::cId,
                                            bsoncxx::builder::basic::make_document(
                                                    bsoncxx::builder::basic::kvp(
                                                            constants::results_cache::search::
                                                                    cArchiveId,
                                                            std::move(result.archive_id)
                                                    ),
                                                    bsoncxx::builder::basic::kvp(
                                                            constants::results_cache::search::
                                                                    cLogEventIdx,
                                                            result.log_event_idx
                                                    )
                                            )
                                    ),
                                    bsoncxx::builder::basic::kvp(
                                            constants::results_cache::search::cOrigFilePath,
                                            std::move(result.original_path)
                                    ),
                                    bsoncxx::builder::basic::kvp(
                                            constants::results_cache::search::cMessage,
                                            std::move(result.message)
                                    ),
                                    bsoncxx::builder::basic::kvp(
                                            constants::results_cache::search::cTimestamp,
                                            result.timestamp
                                    ),
                                    bsoncxx::builder::basic::kvp(
                                            std::string{constants::results_cache::search::cDataset},
                                            std::move(result.dataset)
                                    )
                            )
                    )
            );
        } catch (mongocxx::exception const& e) {
            SPDLOG_ERROR("Failed to build search result - {}", e.what());
            return ErrorCode::ErrorCodeFailureDbBulkWrite;
        }

        count++;
        if (count == m_batch_size) {
            if (false == insert_results()) {
                return ErrorCode::ErrorCodeFailureDbBulkWrite;
            }
            count = 0;
        }
    }

    if (false == m_results.empty() && false == insert_results()) {
        return ErrorCode::ErrorCodeFailureDbBulkWrite;
    }
    return ErrorCode::ErrorCodeSuccess;
}

void ResultsCacheOutputHandler::write(
        string_view message,
        epochtime_t timestamp,
        string_view archive_id,
        int64_t log_event_idx
) {
    if (m_latest_results.size() < m_max_num_results) {
        m_latest_results.emplace(
                std::make_unique<QueryResult>(
                        string_view{},
                        message,
                        timestamp,
                        archive_id,
                        log_event_idx,
                        m_dataset
                )
        );
    } else if (m_latest_results.top()->timestamp < timestamp) {
        m_latest_results.pop();
        m_latest_results.emplace(
                std::make_unique<QueryResult>(
                        string_view{},
                        message,
                        timestamp,
                        archive_id,
                        log_event_idx,
                        m_dataset
                )
        );
    }
}

auto ResultsCacheOutputHandler::insert_results() -> bool {
    try {
        m_collection.insert_many(m_results, m_insert_options);
    } catch (mongocxx::bulk_write_exception const& exception) {
        if (false == contains_only_duplicate_key_write_errors(exception, m_results.size())) {
            SPDLOG_ERROR("Failed to insert search results - {}", exception.what());
            return false;
        }
    } catch (mongocxx::exception const& exception) {
        SPDLOG_ERROR("Failed to insert search results - {}", exception.what());
        return false;
    }
    m_results.clear();
    return true;
}

CountReducerOutputHandler::CountReducerOutputHandler(int reducer_socket_fd)
        : search::OutputHandler(false, false),
          m_reducer_socket_fd(reducer_socket_fd),
          m_pipeline(reducer::PipelineInputMode::InterStage) {
    m_pipeline.add_pipeline_stage(std::make_shared<reducer::CountOperator>());
}

auto CountReducerOutputHandler::write(string_view message) -> void {
    m_pipeline.push_record(reducer::EmptyRecord{});
}

auto CountReducerOutputHandler::finish() -> ErrorCode {
    if (false
        == reducer::send_pipeline_results(m_reducer_socket_fd, std::move(m_pipeline.finish())))
    {
        return ErrorCode::ErrorCodeFailureNetwork;
    }
    return ErrorCode::ErrorCodeSuccess;
}

auto CountByTimeReducerOutputHandler::finish() -> ErrorCode {
    if (false
        == reducer::send_pipeline_results(
                m_reducer_socket_fd,
                std::make_unique<reducer::Int64Int64MapRecordGroupIterator>(
                        m_bucket_counts,
                        reducer::CountOperator::cRecordElementKey
                )
        ))
    {
        return ErrorCode::ErrorCodeFailureNetwork;
    }
    return ErrorCode::ErrorCodeSuccess;
}
}  // namespace clp_s
