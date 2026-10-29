#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <chrono>
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

struct PollOutcome {
    int ret;
    short revents {0};
};

// poll(2) 中 POLLHUP/POLLERR 总是上报，POLLRDHUP 需要显式请求。
constexpr int kPollEvents = POLLIN | POLLOUT | POLLRDHUP;

PollOutcome PollNow(int socket_fd, int events) {
    pollfd descriptor {};
    descriptor.fd = socket_fd;
    descriptor.events = static_cast<short>(events);
    const int ret = poll(&descriptor, 1, 0);
    PollOutcome outcome;
    outcome.ret = ret;
    outcome.revents = descriptor.revents;
    return outcome;
}

void WaitForEvent(int socket_fd, short event) {
    // Request only the prerequisite event: POLLOUT or already queued POLLIN
    // would return immediately while a FIN is still travelling through TCP.
    pollfd descriptor {};
    descriptor.fd = socket_fd;
    descriptor.events = event;
    ASSERT_EQ(poll(&descriptor, 1, 5000), 1)
        << "waiting for event=" << event << ": " << ErrnoString(errno);
    ASSERT_NE(descriptor.revents & event, 0)
        << "requested event=" << event << ", revents=" << descriptor.revents;
}

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
    ASSERT_NO_FATAL_FAILURE(WaitForEvent(socket_fd.Get(), POLLIN));
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
    // A successful send queues bytes; Nagle/delayed ACK may defer delivery.
    ASSERT_NO_FATAL_FAILURE(WaitForEvent(socket_fd.Get(), POLLIN));
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
    // send() publishes output; asynchronous local delivery may not yet have
    // populated the receive queue. This test exercises already queued data.
    ASSERT_NO_FATAL_FAILURE(WaitForEvent(socket_fd.Get(), POLLIN));
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
    ASSERT_NO_FATAL_FAILURE(WaitForEvent(recv_socket_fd.Get(), POLLIN));
    ASSERT_EQ(shutdown(recv_socket_fd.Get(), SHUT_RD), 0)
        << "shutdown(SHUT_RD) before recv failed: " << ErrnoString(errno);
    ASSERT_EQ(recv(recv_socket_fd.Get(), buffer.data(), buffer.size(), 0),
              static_cast<ssize_t>(kPayload.size()))
        << "SHUT_RD exhaustion must not overwrite progress from this recv: "
        << ErrnoString(errno);
    EXPECT_TRUE(std::equal(kPayload.begin(), kPayload.end(), buffer.begin()));
    EXPECT_EQ(recv(recv_socket_fd.Get(), buffer.data(), buffer.size(), 0), 0);
}

// SHUT_RD is not a frozen receive quota: Linux still reads subsequently queued
// data before reporting EOF. Use a separate peer so delivery is independent of
// the shutdown endpoint and verify actual queue length, not EOF's POLLIN bit.
TEST_P(TcpSelfConnectSemantics, ReceiveShutdownReadsLaterPeerData) {
    const int family = GetParam();
    FdGuard listener(socket(family, SOCK_STREAM, 0));
    ASSERT_GE(listener.Get(), 0);
    sockaddr_storage storage{};
    socklen_t size;
    if (family == AF_INET) {
        auto* address = reinterpret_cast<sockaddr_in*>(&storage);
        address->sin_family = family;
        address->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        size = sizeof(*address);
    } else {
        auto* address = reinterpret_cast<sockaddr_in6*>(&storage);
        address->sin6_family = family;
        address->sin6_addr = in6addr_loopback;
        size = sizeof(*address);
    }
    ASSERT_EQ(bind(listener.Get(), reinterpret_cast<sockaddr*>(&storage), size), 0);
    ASSERT_EQ(listen(listener.Get(), 1), 0);
    ASSERT_EQ(getsockname(listener.Get(), reinterpret_cast<sockaddr*>(&storage), &size), 0);
    FdGuard client(socket(family, SOCK_STREAM | SOCK_NONBLOCK, 0));
    ASSERT_GE(client.Get(), 0);
    if (connect(client.Get(), reinterpret_cast<sockaddr*>(&storage), size) != 0) {
        ASSERT_EQ(errno, EINPROGRESS);
    }
    ASSERT_NO_FATAL_FAILURE(WaitForEvent(listener.Get(), POLLIN));
    FdGuard peer(accept(listener.Get(), nullptr, nullptr));
    ASSERT_GE(peer.Get(), 0);
    ASSERT_NO_FATAL_FAILURE(WaitForEvent(client.Get(), POLLOUT));
    int error = -1;
    size = sizeof(error);
    ASSERT_EQ(getsockopt(client.Get(), SOL_SOCKET, SO_ERROR, &error, &size), 0);
    ASSERT_EQ(error, 0);
    ASSERT_EQ(shutdown(client.Get(), SHUT_RD), 0);
    std::array<char, 16> buffer{};
    EXPECT_EQ(read(client.Get(), buffer.data(), buffer.size()), 0);
    EXPECT_EQ(recv(client.Get(), buffer.data(), buffer.size(), MSG_DONTWAIT), 0);

    auto await_queued = [&](int expected) {
        const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(3);
        int queued = 0;
        do {
            ASSERT_EQ(ioctl(client.Get(), FIONREAD, &queued), 0);
            if (queued >= expected) return;
            usleep(1000);
        } while (std::chrono::steady_clock::now() < deadline);
        FAIL() << "queued=" << queued << ", expected=" << expected;
    };
    constexpr char payload[] = "abcdef";
    ASSERT_EQ(send(peer.Get(), payload, 6, MSG_NOSIGNAL), 6);
    ASSERT_NO_FATAL_FAILURE(await_queued(6));
    ASSERT_EQ(recv(client.Get(), buffer.data(), 2, MSG_PEEK | MSG_DONTWAIT), 2);
    EXPECT_EQ(std::string(buffer.data(), 2), "ab");
    buffer.fill('!');
    ASSERT_EQ(recv(client.Get(), buffer.data(), 2, MSG_TRUNC | MSG_DONTWAIT), 2);
    EXPECT_EQ(buffer[0], '!');
    EXPECT_EQ(buffer[1], '!');
    iovec vectors[]{{buffer.data(), 2}, {buffer.data() + 2, 8}};
    ASSERT_EQ(readv(client.Get(), vectors, 2), 4);
    EXPECT_EQ(std::string(buffer.data(), 4), "cdef");
    EXPECT_EQ(read(client.Get(), buffer.data(), buffer.size()), 0);

    ASSERT_EQ(send(peer.Get(), payload, 6, MSG_NOSIGNAL), 6);
    ASSERT_NO_FATAL_FAILURE(await_queued(6));
    ASSERT_EQ(recv(client.Get(), buffer.data(), buffer.size(), MSG_DONTWAIT), 6);
    EXPECT_EQ(std::string(buffer.data(), 6), "abcdef");
    EXPECT_EQ(recv(client.Get(), buffer.data(), buffer.size(), MSG_DONTWAIT), 0);
}

// self-connect 建立完成后，poll() 不得报告任何挂断位。
// 套接字在 bind() 窗口期会被 iface 打上 POLLHUP，每次 poll 都必须按当前状态重算并清掉它，
// 否则 poll(fd, POLLIN, 0) 会在整个连接生命周期内恒返回 1。
TEST_P(TcpSelfConnectSemantics, PollReportsNoHangupWhileConnectionIsOpen) {
    FdGuard socket_fd = CreateSelfConnectedSocket(GetParam());
    ASSERT_GE(socket_fd.Get(), 0);

    const PollOutcome outcome = PollNow(socket_fd.Get(), kPollEvents);
    ASSERT_EQ(outcome.ret, 1) << "poll revents=" << outcome.revents;
    EXPECT_NE(outcome.revents & POLLOUT, 0) << "revents=" << outcome.revents;
    EXPECT_EQ(outcome.revents & (POLLHUP | POLLRDHUP | POLLERR), 0)
        << "已建立的 self-connect 不得残留挂断位: revents=" << outcome.revents;
}

// self-connect 发出的 FIN 会回到自己（Linux tcp_fin() 收到 FIN 时置 RCV_SHUTDOWN），
// 因此 SHUT_WR 之后本端 sk_shutdown 已经是 SHUTDOWN_MASK：
// Linux 6.6 tcp_poll() 会同时报告 POLLRDHUP 与 POLLHUP（实测 revents=IN|OUT|HUP|RDHUP）。
// 已排队的字节仍然可读，挂断位与“数据可读”不互斥。
TEST_P(TcpSelfConnectSemantics, PollWriteShutdownReportsFullHangup) {
    FdGuard socket_fd = CreateSelfConnectedSocket(GetParam());
    ASSERT_GE(socket_fd.Get(), 0);

    constexpr std::array<std::uint8_t, 6> kPayload {1, 2, 3, 4, 5, 6};
    std::array<std::uint8_t, 16> buffer {};
    ASSERT_EQ(send(socket_fd.Get(), kPayload.data(), kPayload.size(), 0),
              static_cast<ssize_t>(kPayload.size()))
        << "send before shutdown failed: " << ErrnoString(errno);
    ASSERT_EQ(shutdown(socket_fd.Get(), SHUT_WR), 0)
        << "shutdown(SHUT_WR) failed: " << ErrnoString(errno);

    // The receive shutdown becomes visible only after our FIN loops back.
    ASSERT_NO_FATAL_FAILURE(WaitForEvent(socket_fd.Get(), POLLRDHUP));
    const PollOutcome outcome = PollNow(socket_fd.Get(), kPollEvents);
    ASSERT_EQ(outcome.ret, 1) << "poll revents=" << outcome.revents;
    EXPECT_NE(outcome.revents & POLLIN, 0) << "revents=" << outcome.revents;
    EXPECT_NE(outcome.revents & POLLOUT, 0)
        << "SHUT_WR 之后必须报告 POLLOUT，send() 才能被唤醒并返回 EPIPE: revents="
        << outcome.revents;
    EXPECT_NE(outcome.revents & POLLRDHUP, 0) << "revents=" << outcome.revents;
    EXPECT_NE(outcome.revents & POLLHUP, 0)
        << "自身 FIN 回环等价于双向关闭，必须报告 POLLHUP: revents=" << outcome.revents;
    EXPECT_EQ(outcome.revents & POLLERR, 0) << "revents=" << outcome.revents;

    ASSERT_EQ(read(socket_fd.Get(), buffer.data(), buffer.size()),
              static_cast<ssize_t>(kPayload.size()))
        << "SHUT_WR 不得丢弃已排队的读侧数据: " << ErrnoString(errno);
    EXPECT_TRUE(std::equal(kPayload.begin(), kPayload.end(), buffer.begin()));
    EXPECT_EQ(read(socket_fd.Get(), buffer.data(), buffer.size()), 0);
}

TEST_P(TcpSelfConnectSemantics, WriteShutdownFlushesCorkedDataBeforeEof) {
    FdGuard socket_fd = CreateSelfConnectedSocket(GetParam());
    ASSERT_GE(socket_fd.Get(), 0);

    constexpr std::array<std::uint8_t, 6> payload {1, 2, 3, 4, 5, 6};
    ASSERT_EQ(send(socket_fd.Get(), payload.data(), payload.size(), MSG_MORE),
              static_cast<ssize_t>(payload.size()));
    ASSERT_EQ(shutdown(socket_fd.Get(), SHUT_WR), 0) << ErrnoString(errno);
    ASSERT_NO_FATAL_FAILURE(WaitForEvent(socket_fd.Get(), POLLRDHUP));

    std::array<std::uint8_t, 16> buffer {};
    ASSERT_EQ(recv(socket_fd.Get(), buffer.data(), buffer.size(), MSG_DONTWAIT),
              static_cast<ssize_t>(payload.size()));
    EXPECT_TRUE(std::equal(payload.begin(), payload.end(), buffer.begin()));
    EXPECT_EQ(recv(socket_fd.Get(), buffer.data(), buffer.size(), MSG_DONTWAIT), 0);
    EXPECT_EQ(send(socket_fd.Get(), payload.data(), payload.size(), MSG_NOSIGNAL), -1);
    EXPECT_EQ(errno, EPIPE);
}

// SHUT_RD 只是半关闭：Linux 6.6 tcp_poll() 此时只置 RCV_SHUTDOWN，
// 报告 POLLRDHUP（以及 POLLIN），不得报告 POLLHUP。
TEST_P(TcpSelfConnectSemantics, PollReadShutdownReportsHalfClose) {
    FdGuard socket_fd = CreateSelfConnectedSocket(GetParam());
    ASSERT_GE(socket_fd.Get(), 0);

    ASSERT_EQ(shutdown(socket_fd.Get(), SHUT_RD), 0)
        << "shutdown(SHUT_RD) failed: " << ErrnoString(errno);

    const PollOutcome outcome = PollNow(socket_fd.Get(), kPollEvents);
    ASSERT_EQ(outcome.ret, 1) << "poll revents=" << outcome.revents;
    EXPECT_NE(outcome.revents & POLLIN, 0) << "revents=" << outcome.revents;
    EXPECT_NE(outcome.revents & POLLRDHUP, 0)
        << "SHUT_RD 关闭读侧，必须报告 POLLRDHUP: revents=" << outcome.revents;
    EXPECT_EQ(outcome.revents & POLLHUP, 0)
        << "SHUT_RD 只是半关闭，不得报告 POLLHUP: revents=" << outcome.revents;
    EXPECT_EQ(outcome.revents & POLLERR, 0) << "revents=" << outcome.revents;
}

// SHUT_RDWR 之后 sk_shutdown == SHUTDOWN_MASK，Linux 报告 POLLHUP|POLLRDHUP。
TEST_P(TcpSelfConnectSemantics, PollDoubleShutdownReportsFullHangup) {
    FdGuard socket_fd = CreateSelfConnectedSocket(GetParam());
    ASSERT_GE(socket_fd.Get(), 0);

    ASSERT_EQ(shutdown(socket_fd.Get(), SHUT_RDWR), 0)
        << "shutdown(SHUT_RDWR) failed: " << ErrnoString(errno);

    const PollOutcome outcome = PollNow(socket_fd.Get(), kPollEvents);
    ASSERT_EQ(outcome.ret, 1) << "poll revents=" << outcome.revents;
    EXPECT_NE(outcome.revents & POLLRDHUP, 0) << "revents=" << outcome.revents;
    EXPECT_NE(outcome.revents & POLLHUP, 0) << "revents=" << outcome.revents;
    EXPECT_EQ(outcome.revents & POLLERR, 0) << "revents=" << outcome.revents;
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
