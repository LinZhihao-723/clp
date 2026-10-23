#include <netinet/in.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

#include <array>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <string>
#include <string_view>
#include <thread>
#include <utility>
#include <vector>

#include <catch2/catch_test_macros.hpp>
#include <catch2/generators/catch_generators.hpp>
#include <msgpack.hpp>

#include "../src/clp_s/ErrorCode.hpp"
#include "../src/clp_s/OutputHandlerImpl.hpp"

namespace {
using clp_s::ErrorCode;
using clp_s::NetworkOutputHandler;

constexpr std::string_view cLoopbackHost{"127.0.0.1"};
constexpr std::string_view cSessionToken{"test-session-token"};
constexpr uint64_t cTaskIdx{7};
constexpr std::string_view cArchiveId{"test-archive-id"};
constexpr std::string_view cMessagePrefix{"message-"};
constexpr int64_t cTimestampBase{1'700'000'000'000};
constexpr uint8_t cExpectedProtocolVersion{2};
constexpr uint32_t cNumHandshakeFields{4};
constexpr uint32_t cNumResultFields{3};
constexpr std::chrono::seconds cListenerTimeout{5};
constexpr std::chrono::milliseconds cNoConnectionWaitTime{200};

/**
 * What a `LoopbackListener` observed on the connection it accepted.
 */
struct ReceivedStream {
    bool accepted_connection{false};
    bool received_eof{false};
    std::string bytes;
};

/**
 * A single-connection TCP listener on the loopback interface. It waits for one connection (up to a
 * timeout), reads until EOF (or a timeout), optionally waits, writes a reply, and then closes the
 * connection.
 */
class LoopbackListener {
public:
    // Constructors
    LoopbackListener() {
        m_listen_fd = socket(AF_INET, SOCK_STREAM, 0);
        REQUIRE((-1 != m_listen_fd));

        sockaddr_in address{};
        address.sin_family = AF_INET;
        address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        address.sin_port = 0;
        // NOLINTNEXTLINE(cppcoreguidelines-pro-type-reinterpret-cast): Required by the BSD API.
        auto* const generic_address{reinterpret_cast<sockaddr*>(&address)};
        REQUIRE((0 == bind(m_listen_fd, generic_address, sizeof(address))));
        REQUIRE((0 == listen(m_listen_fd, 1)));

        socklen_t address_len{sizeof(address)};
        REQUIRE((0 == getsockname(m_listen_fd, generic_address, &address_len)));
        m_port = ntohs(address.sin_port);
    }

    // Destructor
    ~LoopbackListener() {
        if (m_thread.joinable()) {
            m_thread.join();
        }
        close(m_listen_fd);
    }

    // Delete copy & move constructors and assignment operators
    LoopbackListener(LoopbackListener const&) = delete;
    LoopbackListener(LoopbackListener&&) = delete;
    auto operator=(LoopbackListener const&) -> LoopbackListener& = delete;
    auto operator=(LoopbackListener&&) -> LoopbackListener& = delete;

    // Methods
    [[nodiscard]] auto get_port() const -> int { return m_port; }

    /**
     * Starts serving a single connection in a background thread.
     *
     * @param reply The bytes to write after the producer's EOF.
     * @param reply_delay How long to wait after the producer's EOF before writing the reply.
     * @param accept_timeout How long to wait for the connection before giving up.
     */
    auto start(
            std::string reply,
            std::chrono::milliseconds reply_delay,
            std::chrono::milliseconds accept_timeout = cListenerTimeout
    ) -> void {
        m_thread = std::thread{
                [this, reply{std::move(reply)}, reply_delay, accept_timeout]() -> void {
                    serve(reply, reply_delay, accept_timeout);
                }
        };
    }

    /**
     * Waits for the background thread to finish serving the connection.
     *
     * @return What the listener observed on the connection.
     */
    [[nodiscard]] auto join() -> ReceivedStream {
        m_thread.join();
        return std::move(m_received_stream);
    }

private:
    // Methods
    auto serve(
            std::string_view reply,
            std::chrono::milliseconds reply_delay,
            std::chrono::milliseconds accept_timeout
    ) -> void {
        pollfd poll_fd{.fd = m_listen_fd, .events = POLLIN, .revents = 0};
        if (1 != poll(&poll_fd, 1, static_cast<int>(accept_timeout.count()))) {
            return;
        }
        auto const connection_fd{accept(m_listen_fd, nullptr, nullptr)};
        if (-1 == connection_fd) {
            return;
        }
        m_received_stream.accepted_connection = true;

        timeval const receive_timeout{.tv_sec = cListenerTimeout.count(), .tv_usec = 0};
        setsockopt(
                connection_fd,
                SOL_SOCKET,
                SO_RCVTIMEO,
                &receive_timeout,
                sizeof(receive_timeout)
        );

        constexpr size_t cReadBufferSize{4096};
        std::array<char, cReadBufferSize> read_buffer{};
        while (true) {
            auto const num_bytes_received{
                    recv(connection_fd, read_buffer.data(), read_buffer.size(), 0)
            };
            if (0 == num_bytes_received) {
                m_received_stream.received_eof = true;
                break;
            }
            if (0 > num_bytes_received) {
                break;
            }
            m_received_stream.bytes.append(
                    read_buffer.data(),
                    static_cast<size_t>(num_bytes_received)
            );
        }

        std::this_thread::sleep_for(reply_delay);
        if (false == reply.empty()) {
            send(connection_fd, reply.data(), reply.size(), MSG_NOSIGNAL);
        }
        close(connection_fd);
    }

    // Variables
    int m_listen_fd{-1};
    int m_port{};
    std::thread m_thread;
    ReceivedStream m_received_stream;
};

/**
 * @param num_acked_results
 * @return A well-formed acknowledgement for the given number of results.
 */
[[nodiscard]] auto make_ack(uint64_t num_acked_results) -> std::string {
    constexpr size_t cNumCountBytes{sizeof(uint64_t)};
    constexpr size_t cNumBitsPerByte{8};
    constexpr uint64_t cByteMask{0xff};

    std::string ack{"\x91\xcf"};
    for (size_t i{0}; i < cNumCountBytes; ++i) {
        auto const shift{(cNumCountBytes - 1 - i) * cNumBitsPerByte};
        ack.push_back(static_cast<char>((num_acked_results >> shift) & cByteMask));
    }
    return ack;
}

/**
 * @param num_results
 * @return The handshake and results the handler is expected to send for `num_results` results
 * written by `write_results`, without the trailer.
 */
[[nodiscard]] auto make_expected_handshake_and_results(uint64_t num_results) -> std::string {
    msgpack::sbuffer buffer;
    msgpack::packer<msgpack::sbuffer> packer{buffer};
    packer.pack_array(cNumHandshakeFields);
    packer.pack_uint8(cExpectedProtocolVersion);
    packer.pack(cSessionToken);
    packer.pack_uint64(cTaskIdx);
    packer.pack(cArchiveId);
    for (uint64_t result_idx{0}; result_idx < num_results; ++result_idx) {
        packer.pack_array(cNumResultFields);
        packer.pack_uint64(result_idx);
        packer.pack_int64(cTimestampBase + static_cast<int64_t>(result_idx));
        packer.pack(std::string{cMessagePrefix} + std::to_string(result_idx));
    }
    return {buffer.data(), buffer.size()};
}

auto write_results(NetworkOutputHandler& handler, uint64_t num_results) -> void {
    for (uint64_t result_idx{0}; result_idx < num_results; ++result_idx) {
        handler.write(
                std::string{cMessagePrefix} + std::to_string(result_idx),
                cTimestampBase + static_cast<int64_t>(result_idx),
                cArchiveId,
                0
        );
    }
}

[[nodiscard]] auto make_handler(LoopbackListener const& listener) -> NetworkOutputHandler {
    return NetworkOutputHandler{
            std::string{cLoopbackHost},
            listener.get_port(),
            std::string{cSessionToken},
            cTaskIdx
    };
}
}  // namespace

TEST_CASE("clp-s-network-output-handler-no-results", "[clp-s][network-output-handler]") {
    LoopbackListener listener;
    listener.start(std::string{}, std::chrono::milliseconds{0}, cNoConnectionWaitTime);
    auto handler{make_handler(listener)};

    REQUIRE((ErrorCode::ErrorCodeSuccess == handler.finish()));
    REQUIRE_FALSE(listener.join().accepted_connection);
}

TEST_CASE("clp-s-network-output-handler-acked", "[clp-s][network-output-handler]") {
    auto const [num_results, expected_trailer] = GENERATE(
            std::pair<uint64_t, std::string>{1, std::string{"\x91\x01"}},
            std::pair<uint64_t, std::string>{3, std::string{"\x91\x03"}},
            std::pair<uint64_t, std::string>{200, std::string{"\x91\xcc\xc8"}},
            std::pair<uint64_t, std::string>{300, std::string{"\x91\xcd\x01\x2c"}}
    );

    LoopbackListener listener;
    listener.start(make_ack(num_results), std::chrono::milliseconds{0});
    {
        auto handler{make_handler(listener)};
        write_results(handler, num_results);
        REQUIRE((ErrorCode::ErrorCodeSuccess == handler.finish()));
    }

    auto const received_stream{listener.join()};
    REQUIRE(received_stream.accepted_connection);
    REQUIRE(received_stream.received_eof);
    REQUIRE(
            (make_expected_handshake_and_results(num_results) + expected_trailer
             == received_stream.bytes)
    );
}

TEST_CASE("clp-s-network-output-handler-delayed-ack", "[clp-s][network-output-handler]") {
    constexpr uint64_t cNumResults{2};
    constexpr std::chrono::milliseconds cAckDelay{1500};

    LoopbackListener listener;
    listener.start(make_ack(cNumResults), cAckDelay);
    auto handler{make_handler(listener)};
    write_results(handler, cNumResults);

    auto const finish_begin{std::chrono::steady_clock::now()};
    REQUIRE((ErrorCode::ErrorCodeSuccess == handler.finish()));
    REQUIRE((cAckDelay <= std::chrono::steady_clock::now() - finish_begin));

    auto const received_stream{listener.join()};
    REQUIRE(received_stream.received_eof);
}

TEST_CASE("clp-s-network-output-handler-not-acked", "[clp-s][network-output-handler]") {
    constexpr uint64_t cNumResults{3};

    auto const reply = GENERATE(
            // Closes the connection without acknowledging.
            std::string{},
            // Acknowledges a different number of results.
            make_ack(cNumResults + 1),
            make_ack(cNumResults - 1),
            // Acknowledges with a malformed array header or integer marker.
            std::string{"\x92"} + make_ack(cNumResults).substr(1),
            std::string{"\x91\xce"} + make_ack(cNumResults).substr(2),
            // Closes the connection mid-acknowledgement.
            make_ack(cNumResults).substr(0, 5)
    );

    LoopbackListener listener;
    listener.start(reply, std::chrono::milliseconds{0});
    auto handler{make_handler(listener)};
    write_results(handler, cNumResults);
    REQUIRE((ErrorCode::ErrorCodeFailureNetwork == handler.finish()));

    auto const received_stream{listener.join()};
    REQUIRE(received_stream.received_eof);
    REQUIRE(
            (make_expected_handshake_and_results(cNumResults) + std::string{"\x91\x03"}
             == received_stream.bytes)
    );
}

TEST_CASE("clp-s-network-output-handler-dropped", "[clp-s][network-output-handler]") {
    constexpr uint64_t cNumResults{3};

    LoopbackListener listener;
    listener.start(std::string{}, std::chrono::milliseconds{0});
    {
        auto handler{make_handler(listener)};
        write_results(handler, cNumResults);
    }

    auto const received_stream{listener.join()};
    REQUIRE(received_stream.received_eof);
    REQUIRE((make_expected_handshake_and_results(cNumResults) == received_stream.bytes));
}
