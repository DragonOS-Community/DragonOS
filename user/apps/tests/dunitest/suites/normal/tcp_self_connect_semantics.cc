#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <pthread.h>
#include <sys/socket.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cstdint>
#include <cstring>
#include <string>
#include <vector>

namespace {

class FdGuard {
  public:
    explicit FdGuard(int fd = -1) : fd_(fd) {}
    FdGuard(const FdGuard&) = delete;
    FdGuard& operator=(const FdGuard&) = delete;
    FdGuard(FdGuard&& other) noexcept : fd_(other.fd_) { other.fd_ = -1; }
    FdGuard& operator=(FdGuard&&) = delete;
    ~FdGuard() {
        if (fd_ >= 0) {
            close(fd_);
        }
    }

    int Get() const { return fd_; }

  private:
    int fd_;
};

std::string ErrnoString(int error) {
    return std::to_string(error) + " (" + std::strerror(error) + ")";
}

FdGuard CreateSelfConnectedSocket(int family) {
    FdGuard socket_fd(socket(family, SOCK_STREAM, 0));
    if (socket_fd.Get() < 0) {
        ADD_FAILURE() << "socket failed: " << ErrnoString(errno);
        return FdGuard();
    }

    if (family == AF_INET) {
        sockaddr_in address {};
        address.sin_family = AF_INET;
        address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        address.sin_port = 0;
        if (bind(socket_fd.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)) != 0) {
            ADD_FAILURE() << "bind(AF_INET) failed: " << ErrnoString(errno);
            return FdGuard();
        }
        socklen_t address_len = sizeof(address);
        if (getsockname(socket_fd.Get(), reinterpret_cast<sockaddr*>(&address), &address_len) != 0) {
            ADD_FAILURE() << "getsockname(AF_INET) failed: " << ErrnoString(errno);
            return FdGuard();
        }
        if (connect(socket_fd.Get(), reinterpret_cast<sockaddr*>(&address), address_len) != 0) {
            ADD_FAILURE() << "self-connect(AF_INET) failed: " << ErrnoString(errno);
            return FdGuard();
        }
    } else {
        sockaddr_in6 address {};
        address.sin6_family = AF_INET6;
        address.sin6_addr = in6addr_loopback;
        address.sin6_port = 0;
        if (bind(socket_fd.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)) != 0) {
            ADD_FAILURE() << "bind(AF_INET6) failed: " << ErrnoString(errno);
            return FdGuard();
        }
        socklen_t address_len = sizeof(address);
        if (getsockname(socket_fd.Get(), reinterpret_cast<sockaddr*>(&address), &address_len) != 0) {
            ADD_FAILURE() << "getsockname(AF_INET6) failed: " << ErrnoString(errno);
            return FdGuard();
        }
        if (connect(socket_fd.Get(), reinterpret_cast<sockaddr*>(&address), address_len) != 0) {
            ADD_FAILURE() << "self-connect(AF_INET6) failed: " << ErrnoString(errno);
            return FdGuard();
        }
    }

    return socket_fd;
}

class TcpSelfConnectSemantics : public testing::TestWithParam<int> {};

struct ReaderResult {
    int socket_fd;
    pthread_barrier_t* start;
    std::uint8_t expected_byte;
    std::size_t bytes_read {0};
    int error {0};
    bool saw_eof {false};
    bool data_mismatch {false};
};

void* ReadSelfConnectedStream(void* opaque) {
    auto* result = static_cast<ReaderResult*>(opaque);
    pthread_barrier_wait(result->start);

    std::array<std::uint8_t, 2500> buffer {};
    for (;;) {
        const ssize_t count = read(result->socket_fd, buffer.data(), buffer.size());
        if (count > 0) {
            result->bytes_read += static_cast<std::size_t>(count);
            result->data_mismatch |= !std::all_of(
                buffer.begin(), buffer.begin() + count,
                [result](std::uint8_t byte) { return byte == result->expected_byte; });
            continue;
        }
        if (count == 0) {
            result->saw_eof = true;
            return nullptr;
        }
        if (errno == EINTR) {
            continue;
        }
        result->error = errno;
        return nullptr;
    }
}

struct SendAllResult {
    std::size_t bytes_sent {0};
    int error {0};
};

SendAllResult SendAll(int socket_fd, const std::uint8_t* data, std::size_t length) {
    SendAllResult result;
    while (result.bytes_sent < length) {
        const ssize_t count =
            send(socket_fd, data + result.bytes_sent, length - result.bytes_sent, 0);
        if (count > 0) {
            result.bytes_sent += static_cast<std::size_t>(count);
            continue;
        }
        if (count == 0) {
            result.error = EIO;
            break;
        }
        if (errno == EINTR) {
            continue;
        }
        result.error = errno;
        break;
    }
    return result;
}

TEST_P(TcpSelfConnectSemantics, PartialProgressWinsOverWouldBlock) {
    FdGuard socket_fd = CreateSelfConnectedSocket(GetParam());
    ASSERT_GE(socket_fd.Get(), 0);

    const int original_flags = fcntl(socket_fd.Get(), F_GETFL, 0);
    ASSERT_GE(original_flags, 0) << "fcntl(F_GETFL) failed: " << ErrnoString(errno);
    ASSERT_EQ(fcntl(socket_fd.Get(), F_SETFL, original_flags | O_NONBLOCK), 0)
        << "fcntl(F_SETFL, O_NONBLOCK) failed: " << ErrnoString(errno);

    constexpr std::array<std::uint8_t, 5> kReadPayload {0x11, 0x22, 0x33, 0x44, 0x55};
    std::array<std::uint8_t, 16> buffer {};

    ASSERT_EQ(send(socket_fd.Get(), kReadPayload.data(), kReadPayload.size(), 0),
              static_cast<ssize_t>(kReadPayload.size()))
        << "send before read failed: " << ErrnoString(errno);
    ASSERT_EQ(read(socket_fd.Get(), buffer.data(), buffer.size()),
              static_cast<ssize_t>(kReadPayload.size()))
        << "read must return progress copied before the second probe observed EAGAIN: "
        << ErrnoString(errno);
    EXPECT_TRUE(std::equal(kReadPayload.begin(), kReadPayload.end(), buffer.begin()));

    errno = 0;
    EXPECT_EQ(read(socket_fd.Get(), buffer.data(), buffer.size()), -1);
    EXPECT_TRUE(errno == EAGAIN || errno == EWOULDBLOCK) << ErrnoString(errno);

    constexpr std::array<std::uint8_t, 7> kRecvPayload {0xa1, 0xa2, 0xa3, 0xa4,
                                                        0xa5, 0xa6, 0xa7};
    buffer.fill(0);
    ASSERT_EQ(send(socket_fd.Get(), kRecvPayload.data(), kRecvPayload.size(), 0),
              static_cast<ssize_t>(kRecvPayload.size()))
        << "send before recv failed: " << ErrnoString(errno);
    ASSERT_EQ(recv(socket_fd.Get(), buffer.data(), buffer.size(), MSG_DONTWAIT),
              static_cast<ssize_t>(kRecvPayload.size()))
        << "recv must return progress copied before the second probe observed EAGAIN: "
        << ErrnoString(errno);
    EXPECT_TRUE(std::equal(kRecvPayload.begin(), kRecvPayload.end(), buffer.begin()));
}

TEST_P(TcpSelfConnectSemantics, ReceiveShutdownDoesNotEraseCurrentReadProgress) {
    FdGuard socket_fd = CreateSelfConnectedSocket(GetParam());
    ASSERT_GE(socket_fd.Get(), 0);

    constexpr std::array<std::uint8_t, 6> kPayload {1, 2, 3, 4, 5, 6};
    std::array<std::uint8_t, 16> buffer {};
    ASSERT_EQ(send(socket_fd.Get(), kPayload.data(), kPayload.size(), 0),
              static_cast<ssize_t>(kPayload.size()))
        << "send before shutdown failed: " << ErrnoString(errno);
    ASSERT_EQ(shutdown(socket_fd.Get(), SHUT_RD), 0)
        << "shutdown(SHUT_RD) failed: " << ErrnoString(errno);

    ASSERT_EQ(read(socket_fd.Get(), buffer.data(), buffer.size()),
              static_cast<ssize_t>(kPayload.size()))
        << "SHUT_RD exhaustion must not overwrite progress from this read: " << ErrnoString(errno);
    EXPECT_TRUE(std::equal(kPayload.begin(), kPayload.end(), buffer.begin()));
    EXPECT_EQ(read(socket_fd.Get(), buffer.data(), buffer.size()), 0);

    FdGuard recv_socket_fd = CreateSelfConnectedSocket(GetParam());
    ASSERT_GE(recv_socket_fd.Get(), 0);
    buffer.fill(0);
    ASSERT_EQ(send(recv_socket_fd.Get(), kPayload.data(), kPayload.size(), 0),
              static_cast<ssize_t>(kPayload.size()))
        << "send before recv shutdown failed: " << ErrnoString(errno);
    ASSERT_EQ(shutdown(recv_socket_fd.Get(), SHUT_RD), 0)
        << "shutdown(SHUT_RD) before recv failed: " << ErrnoString(errno);
    ASSERT_EQ(recv(recv_socket_fd.Get(), buffer.data(), buffer.size(), 0),
              static_cast<ssize_t>(kPayload.size()))
        << "SHUT_RD exhaustion must not overwrite progress from this recv: "
        << ErrnoString(errno);
    EXPECT_TRUE(std::equal(kPayload.begin(), kPayload.end(), buffer.begin()));
    EXPECT_EQ(recv(recv_socket_fd.Get(), buffer.data(), buffer.size(), 0), 0);
}

TEST_P(TcpSelfConnectSemantics, ConcurrentReadSendAndWriteShutdownCompletes) {
    // This is a bounded in-tree stress/smoke test for the lock-lifetime regression exercised by
    // gVisor SelfConnectSendRecv. The high-count validation remains in the gVisor test because a
    // scheduler race cannot be made deterministic without adding a test-only kernel hook.
    constexpr std::size_t kIterations = 64;
    constexpr std::size_t kPayloadSize = 1U << 20;
    constexpr std::uint8_t kPayloadByte = 0x5a;
    const std::vector<std::uint8_t> payload(kPayloadSize, kPayloadByte);

    for (std::size_t iteration = 0; iteration < kIterations; ++iteration) {
        FdGuard socket_fd = CreateSelfConnectedSocket(GetParam());
        ASSERT_GE(socket_fd.Get(), 0) << "iteration " << iteration;

        pthread_barrier_t start;
        ASSERT_EQ(pthread_barrier_init(&start, nullptr, 2), 0) << "iteration " << iteration;

        ReaderResult reader_result {};
        reader_result.socket_fd = socket_fd.Get();
        reader_result.start = &start;
        reader_result.expected_byte = kPayloadByte;
        pthread_t reader;
        const int create_error =
            pthread_create(&reader, nullptr, ReadSelfConnectedStream, &reader_result);
        if (create_error != 0) {
            pthread_barrier_destroy(&start);
            FAIL() << "pthread_create failed at iteration " << iteration << ": "
                   << ErrnoString(create_error);
        }

        pthread_barrier_wait(&start);
        const SendAllResult send_result =
            SendAll(socket_fd.Get(), payload.data(), payload.size());
        const int shutdown_result = shutdown(socket_fd.Get(), SHUT_WR);
        const int shutdown_error = shutdown_result == 0 ? 0 : errno;
        const int join_error = pthread_join(reader, nullptr);
        const int barrier_error = pthread_barrier_destroy(&start);

        EXPECT_EQ(send_result.error, 0)
            << "send failed at iteration " << iteration << ": "
            << ErrnoString(send_result.error);
        EXPECT_EQ(send_result.bytes_sent, payload.size()) << "iteration " << iteration;
        EXPECT_EQ(shutdown_result, 0)
            << "shutdown(SHUT_WR) failed at iteration " << iteration << ": "
            << ErrnoString(shutdown_error);
        EXPECT_EQ(join_error, 0) << "pthread_join failed at iteration " << iteration;
        EXPECT_EQ(barrier_error, 0) << "pthread_barrier_destroy failed at iteration " << iteration;
        EXPECT_EQ(reader_result.error, 0)
            << "read failed at iteration " << iteration << ": "
            << ErrnoString(reader_result.error);
        EXPECT_TRUE(reader_result.saw_eof) << "iteration " << iteration;
        EXPECT_FALSE(reader_result.data_mismatch) << "iteration " << iteration;
        EXPECT_EQ(reader_result.bytes_read, payload.size()) << "iteration " << iteration;
    }
}

INSTANTIATE_TEST_SUITE_P(IPv4AndIPv6, TcpSelfConnectSemantics,
                         testing::Values(AF_INET, AF_INET6));

} // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
