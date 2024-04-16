#include "StreamingReader.hpp"

#include <chrono>
#include <cstring>
#include <functional>
#include <iterator>
#include <memory>
#include <string>

#include <curl/curl.h>

#include "ErrorCode.hpp"
#include "TraceableException.hpp"

namespace clp {
namespace {
/**
 * The exception thrown by a failed libcurl operation.
 */
class CurlOperationFailed : public TraceableException {
public:
    CurlOperationFailed(
            ErrorCode error_code,
            char const* const filename,
            int line_number,
            CURLcode err
    )
            : m_curl_err{err},
              TraceableException(error_code, filename, line_number) {}

    [[nodiscard]] auto get_curl_err() const -> CURLcode { return m_curl_err; }

    [[nodiscard]] auto what() const noexcept -> char const* override {
        return "clp::<unnamed>::CurlDownloadHandler operation failed.";
    }

private:
    CURLcode m_curl_err;
};

/**
 * This class wraps the C implementation of the Curl string list. It provides a cleaner interface to
 * manage the life cycle of the object with proper error handling.
 */
class CurlStringList {
public:
    // Constructor
    CurlStringList() = default;

    // Copy/Move constructors
    CurlStringList(CurlStringList const&) = delete;
    CurlStringList(CurlStringList&&) = delete;
    auto operator=(CurlStringList const&) -> CurlStringList& = delete;
    auto operator=(CurlStringList&&) -> CurlStringList& = delete;

    // Destructor
    ~CurlStringList() {
        if (nullptr != m_list) {
            curl_slist_free_all(m_list);
        }
    }

    // Methods
    /**
     * Appends a string to the end of the list.
     * @param str
     * @throw CurlOperationFailed if append operation failed.
     */
    auto append(std::string_view str) -> void {
        auto* list_after_appending{curl_slist_append(m_list, str.data())};
        if (nullptr == list_after_appending) {
            throw CurlOperationFailed(ErrorCode_Failure, __FILE__, __LINE__, CURLE_OUT_OF_MEMORY);
        }
        m_list = list_after_appending;
        ++m_size;
    }

    [[nodiscard]] auto get_raw_list() const -> struct curl_slist* { return m_list; }

    [[nodiscard]] auto get_list_size() const -> size_t { return m_size; }

    [[nodiscard]] auto is_empty() const -> bool { return 0 == get_list_size(); }

private:
    size_t m_size{0};
    struct curl_slist* m_list{nullptr};
};

/**
 * This class wraps the C implementation of the Curl handler to perform data downloading. It
 * provides a cleaner interface to manage the life cycle of the object with proper error handling.
 */
class CurlDownloadHandler {
public:
    using ProgressCallbackType
            = std::function<int(void*, curl_off_t, curl_off_t, curl_off_t, curl_off_t)>;
    using WriteCallbackType = std::function<size_t(char*, size_t, size_t, void*)>;

    // Constructor
    CurlDownloadHandler(
            StreamingReader& reader,
            std::string_view src_url,
            long connection_timeout,
            long operation_timeout,
            size_t offset,
            bool disable_caching
    );

    // Methods
    /**
     * Downloads the data. This function returns when the download ends or failed. The status will
     * be indicated in the return value.
     * @return error code returned fro the underlying `curl_easy_perform`.
     */
    [[nodiscard]] auto download() -> CURLcode { return curl_easy_perform(m_handler.get()); }

private:
    /**
     * This class defines a customized deleter for the raw curl handler.
     */
    class CurlHandlerDeleter {
    public:
        auto operator()(CURL* curl_handler) -> void {
            if (nullptr != curl_handler) {
                curl_easy_cleanup(curl_handler);
            }
        }
    };

    /**
     * Wrapper of `curl_easy_setopt`.
     * @tparam ValueType
     * @param option
     * @param value
     * @throw CurlOperationFailed if an error happened.
     */
    template <typename ValueType>
    auto set_option(CURLoption option, ValueType value) -> void {
        if (auto const err{curl_easy_setopt(m_handler.get(), option, value)}; CURLE_OK != err) {
            throw CurlOperationFailed(ErrorCode_Failure, __FILE__, __LINE__, err);
        }
    }

    CurlStringList m_http_header_list;
    std::unique_ptr<CURL, CurlHandlerDeleter> m_handler;
};

/**
 * Casts the given ptr to a StreamingReader object. This wrapper function is used to silence
 * clang-tidy warnings.
 * @param ptr The address of a StreamingReader object.
 * @return a StreamingReader type pointer.
 */
[[nodiscard]] auto get_reader(void* ptr) -> StreamingReader* {
    // NOLINTNEXTLINE(cppcoreguidelines-pro-type-reinterpret-cast)
    return reinterpret_cast<StreamingReader*>(ptr);
}

/**
 * Note: this function must have C linkage since it is a libcurl callback function.
 * libcurl progress callback used only to determine whether to abort the current transfer.
 * Doc: https://curl.se/libcurl/c/CURLOPT_XFERINFOFUNCTION.html
 * @param reader_ptr A pointer to an instance of this class.
 * @param dltotal Unused
 * @param dlnow Unused
 * @param ultotal Unused
 * @param ulnow Unused
 * @return 1 if the transfer was aborted, 0 otherwise.
 */
extern "C" auto curl_download_progress_callback(
        void* reader_ptr,
        [[maybe_unused]] curl_off_t dltotal,
        [[maybe_unused]] curl_off_t dlnow,
        [[maybe_unused]] curl_off_t ultotal,
        [[maybe_unused]] curl_off_t ulnow
) -> int {
    return get_reader(reader_ptr)->is_transfer_aborted() ? 1 : 0;
}

/**
 * Note: this function must have C linkage since it is a libcurl callback function.
 * Write-back callback. This is how the data is written into the fetching buffer.
 * Doc: https://curl.se/libcurl/c/CURLOPT_WRITEFUNCTION.html
 * @param ptr A pointer to an instance of StreamingReader.
 * @param size Number of bytes of the input data.
 * @param nmemb
 * @param reader_ptr A pointer pointing to an instance of StreamingReader.
 * @return Number of bytes transferred (fetched).
 */
extern "C" auto curl_download_write_callback(char* ptr, size_t size, size_t nmemb, void* reader_ptr)
        -> size_t {
    return get_reader(reader_ptr)->write_to_fetching_buffer({ptr, size * nmemb});
}

CurlDownloadHandler::CurlDownloadHandler(
        StreamingReader& reader,
        std::string_view src_url,
        long connection_timeout,
        long operation_timeout,
        size_t offset,
        bool disable_caching
)
        : m_handler{curl_easy_init()} {
    if (nullptr == m_handler) {
        throw CurlOperationFailed(ErrorCode_Failure, __FILE__, __LINE__, CURLE_FAILED_INIT);
    }

    // Setup src url.
    set_option(CURLOPT_URL, src_url.data());

    // Setup progress callback.
    set_option(CURLOPT_XFERINFOFUNCTION, curl_download_progress_callback);
    set_option(CURLOPT_XFERINFODATA, static_cast<void*>(&reader));
    set_option(CURLOPT_NOPROGRESS, 0);

    // Setup write callback.
    set_option(CURLOPT_WRITEFUNCTION, curl_download_write_callback);
    set_option(CURLOPT_WRITEDATA, static_cast<void*>(&reader));

    // Setup timeout.
    set_option(CURLOPT_CONNECTTIMEOUT, connection_timeout);
    set_option(CURLOPT_TIMEOUT, operation_timeout);

    // Setup http headers.
    if (0 != offset) {
        std::string const range{"Range: bytes=" + std::to_string(offset) + "-"};
        m_http_header_list.append(range);
    }
    if (disable_caching) {
        m_http_header_list.append("Cache-Control: no-cache");
        m_http_header_list.append("Pragma: no-cache");
    }
    if (false == m_http_header_list.is_empty()) {
        set_option(CURLOPT_HTTPHEADER, m_http_header_list.get_raw_list());
    }
}
}  // namespace

bool StreamingReader::m_initialized{false};

auto StreamingReader::init() -> ErrorCode {
    if (m_initialized) {
        return ErrorCode_Success;
    }
    if (0 != curl_global_init(CURL_GLOBAL_ALL)) {
        return ErrorCode_Failure;
    }
    m_initialized = true;
    return ErrorCode_Success;
}

auto StreamingReader::deinit() -> void {
    curl_global_cleanup();
    m_initialized = false;
}

auto StreamingReader::transfer_thread_entry(
        StreamingReader& reader,
        size_t offset,
        bool disable_caching
) -> void {
    CURLcode retval{CURLE_FAILED_INIT};
    try {
        CurlDownloadHandler curl_handler{
                reader,
                reader.m_src_url,
                static_cast<long>(reader.m_connection_timeout),
                static_cast<long>(reader.m_operation_timeout),
                offset,
                disable_caching
        };
        retval = curl_handler.download();
    } catch (CurlOperationFailed const& ex) {
        reader.m_curl_return_code = ex.get_curl_err();
        reader.set_status_code(StatusCode::Failed);
        reader.m_transfer_terminated.store(true);
        return;
    }

    reader.commit_fetching_buffer();
    reader.set_status_code((CURLE_OK == retval) ? StatusCode::Finished : StatusCode::Failed);
    reader.m_curl_return_code = retval;
    reader.m_transfer_terminated.store(true);
}

auto StreamingReader::write_to_fetching_buffer(StreamingReader::BufferView data_to_write)
        -> size_t {
    auto const num_bytes_to_write{data_to_write.size()};
    try {
        while (false == data_to_write.empty()) {
            StreamingReader::BufferView fetching_buffer;
            if (false == get_buffer_to_fetch(fetching_buffer)) {
                return 0;
            }
            auto const num_bytes_to_fetch{std::min(fetching_buffer.size(), data_to_write.size())};
            auto const copy_it_begin{data_to_write.begin()};
            auto copy_it_end{data_to_write.begin()};
            std::advance(copy_it_end, num_bytes_to_fetch);
            std::copy(copy_it_begin, copy_it_end, fetching_buffer.begin());
            data_to_write = data_to_write.subspan(num_bytes_to_fetch);
            commit_fetching(num_bytes_to_fetch);
        }
    } catch (TraceableException const& ex) {
        // TODO: add logging to track the exceptions.
        return 0;
    }
    return num_bytes_to_write;
}

auto StreamingReader::open(std::string_view src_url, size_t offset, bool disable_caching) -> void {
    if (false == m_initialized) {
        throw OperationFailed(ErrorCode_NotReady, __FILE__, __LINE__);
    }
    if (StatusCode::NotInit != get_status_code()) {
        throw OperationFailed(ErrorCode_NotReady, __FILE__, __LINE__);
    }
    m_src_url = src_url;
    set_status_code(StatusCode::InProgress);
    m_transfer_thread = std::make_unique<std::thread>(
            transfer_thread_entry,
            std::ref(*this),
            offset,
            disable_caching
    );
}

auto StreamingReader::terminate_current_transfer() -> void {
    if (StatusCode::InProgress != get_status_code()) {
        while (false == is_transfer_terminated()) {
        }
    } else {
        // If control flow reaches here, it means the we need to kill the current connected session.
        // Since the fetcher thread is an async daemon thread, we need to set the flag for aborting
        // and wait for it to terminate.
        // TODO: we could use sleep here instead of actively pulling.
        auto transfer_aborted{is_transfer_aborted()};
        while (false == is_transfer_terminated()) {
            if (transfer_aborted) {
                continue;
            }
            abort_data_transfer();
            transfer_aborted = true;
        }
    }
    m_transfer_thread->join();
}

auto StreamingReader::abort_data_transfer() -> void {
    m_transfer_aborted.store(true);
}

auto StreamingReader::set_fetching_buffer() -> bool {
    std::unique_lock<std::mutex> buffer_resource_lock{m_buffer_resource_mutex};
    if (m_fetching_buffer.has_value()) {
        throw OperationFailed(ErrorCode_Corrupt, __FILE__, __LINE__);
    }
    while (m_num_fetched_buffer >= m_buffer_pool_size) {
        if (m_num_fetched_buffer != m_buffer_pool_size) {
            throw OperationFailed(ErrorCode_Corrupt, __FILE__, __LINE__);
        }
        if (std::cv_status::timeout
            != m_cv_fetcher.wait_for(
                    buffer_resource_lock,
                    std::chrono::milliseconds(cConditionVariableTimeoutMilliSecond)
            ))
        {
            continue;
        }
        if (is_transfer_aborted()) {
            return false;
        }
        if (is_transfer_timedout()) {
            return false;
        }
    }
    m_fetching_buffer.emplace(m_buffer_pool.at(m_curr_fetching_buffer_idx).get(), m_buffer_size);
    return true;
}

auto StreamingReader::commit_fetching_buffer() -> void {
    if (false == m_fetching_buffer.has_value()) {
        return;
    }
    std::unique_lock<std::mutex> const buffer_resource_lock{m_buffer_resource_mutex};
    auto const fetching_buffer{m_fetching_buffer.value()};
    m_fetched_buffer_queue.emplace(fetching_buffer.data(), m_fetching_buffer_pos);
    ++m_num_fetched_buffer;

    m_fetching_buffer.reset();
    m_fetching_buffer_pos = 0;
    ++m_curr_fetching_buffer_idx;
    if (m_curr_fetching_buffer_idx == m_buffer_pool_size) {
        m_curr_fetching_buffer_idx = 0;
    }

    m_cv_reader.notify_all();
}

auto StreamingReader::set_reading_buffer() -> bool {
    std::unique_lock<std::mutex> buffer_resource_lock{m_buffer_resource_mutex};
    if (m_reading_buffer.has_value()) {
        throw OperationFailed(ErrorCode_Corrupt, __FILE__, __LINE__);
    }
    while (m_fetched_buffer_queue.empty()) {
        if (is_transfer_terminated()) {
            return false;
        }
        m_cv_reader.wait_for(
                buffer_resource_lock,
                std::chrono::milliseconds(cConditionVariableTimeoutMilliSecond)
        );
    }
    auto const next_reading_buffer{m_fetched_buffer_queue.front()};
    m_fetched_buffer_queue.pop();
    m_reading_buffer.emplace(next_reading_buffer);
    return true;
}

auto StreamingReader::free_reading_buffer() -> void {
    std::unique_lock<std::mutex> const buffer_resource_lock{m_buffer_resource_mutex};
    m_reading_buffer.reset();
    --m_num_fetched_buffer;
    m_cv_fetcher.notify_all();
}

auto StreamingReader::commit_fetching(size_t num_bytes_fetched) -> void {
    m_fetching_buffer_pos += num_bytes_fetched;
    if (m_fetching_buffer_pos > m_buffer_size) {
        throw OperationFailed(ErrorCode_Corrupt, __FILE__, __LINE__);
    }
    if (m_fetching_buffer_pos == m_buffer_size) {
        commit_fetching_buffer();
    }
}

auto StreamingReader::commit_reading(size_t num_bytes_read) -> void {
    if (false == m_reading_buffer.has_value()) {
        throw OperationFailed(ErrorCode_Corrupt, __FILE__, __LINE__);
    }
    auto const reading_buffer_size{m_reading_buffer.value().size()};
    if (reading_buffer_size < num_bytes_read) {
        throw OperationFailed(ErrorCode_Corrupt, __FILE__, __LINE__);
    }
    auto const num_bytes_left{reading_buffer_size - num_bytes_read};
    if (0 == num_bytes_left) {
        free_reading_buffer();
    } else {
        m_reading_buffer = m_reading_buffer.value().last(num_bytes_left);
    }
}

auto StreamingReader::get_buffer_to_fetch(BufferView& fetching_buffer) -> bool {
    if (false == m_fetching_buffer.has_value()) {
        if (false == set_fetching_buffer() || false == m_fetching_buffer.has_value()) {
            return false;
        }
        fetching_buffer = m_fetching_buffer.value();
        return true;
    }
    fetching_buffer = m_fetching_buffer.value().subspan(
            m_fetching_buffer_pos,
            m_buffer_size - m_fetching_buffer_pos
    );
    return true;
}

auto StreamingReader::reset() -> void {
    if (StatusCode::NotInit == get_status_code()) {
        return;
    }

    if (false == m_transfer_terminated.load()) {
        throw OperationFailed(ErrorCode_Failure, __FILE__, __LINE__);
    }

    m_src_url.clear();
    m_file_pos = 0;

    std::queue<BufferView> empty_buffer_queue;
    m_fetched_buffer_queue.swap(empty_buffer_queue);
    m_num_fetched_buffer = 0;
    m_fetching_buffer_pos = 0;
    m_fetching_buffer.reset();
    m_reading_buffer.reset();

    m_transfer_thread.reset();
    m_transfer_aborted.store(false);
    m_transfer_terminated.store(false);
    set_status_code(StatusCode::NotInit);
    m_curl_return_code.reset();
}

auto StreamingReader::read_from_fetched_buffers(
        size_t num_bytes_to_read,
        size_t& num_bytes_read,
        char* dst
) -> ErrorCode {
    num_bytes_read = 0;
    std::optional<BufferView> dst_view{std::nullopt};
    if (nullptr != dst) {
        dst_view = BufferView{dst, num_bytes_to_read};
    }
    while (0 != num_bytes_to_read) {
        if (false == m_reading_buffer.has_value()) {
            if (false == set_reading_buffer() || false == m_reading_buffer.has_value()) {
                return ErrorCode_EndOfFile;
            }
        }
        auto const reading_buffer{m_reading_buffer.value()};
        auto const reading_buffer_size{reading_buffer.size()};
        if (0 == reading_buffer_size) {
            return ErrorCode_EndOfFile;
        }
        auto const num_bytes_to_consume_from_buffer{
                num_bytes_to_read > reading_buffer_size ? reading_buffer_size : num_bytes_to_read
        };
        if (dst_view.has_value()) {
            memcpy(dst_view.value().last(num_bytes_to_read).data(),
                   reading_buffer.data(),
                   num_bytes_to_consume_from_buffer);
        }
        num_bytes_to_read -= num_bytes_to_consume_from_buffer;
        num_bytes_read += num_bytes_to_consume_from_buffer;
        m_file_pos += num_bytes_to_consume_from_buffer;
        commit_reading(num_bytes_to_consume_from_buffer);
    }
    return ErrorCode_Success;
}
}  // namespace clp
