#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>

#include <chrono>
#include <thread>
#include <vector>

namespace {

class Fd {
 public:
    ~Fd() { Reset(); }
    Fd() = default;
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int Get() const { return fd_; }
    void Reset(int fd = -1) {
        if (fd_ >= 0) close(fd_);
        fd_ = fd;
    }
 private:
    int fd_ = -1;
};

constexpr short kReady = POLLIN | POLLRDNORM | POLLOUT | POLLWRNORM | POLLRDHUP;

class TcpPendingError : public ::testing::TestWithParam<int> {
 protected:
    void SetUp() override {
        Fd listener;
        listener.Reset(socket(GetParam(), SOCK_STREAM, 0));
        ASSERT_GE(listener.Get(), 0) << errno;
        sockaddr_storage storage{};
        socklen_t length;
        if (GetParam() == AF_INET) {
            auto* addr = reinterpret_cast<sockaddr_in*>(&storage);
            addr->sin_family = AF_INET;
            addr->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
            length = sizeof(*addr);
        } else {
            auto* addr = reinterpret_cast<sockaddr_in6*>(&storage);
            addr->sin6_family = AF_INET6;
            addr->sin6_addr = in6addr_loopback;
            length = sizeof(*addr);
        }
        auto* addr = reinterpret_cast<sockaddr*>(&storage);
        ASSERT_EQ(0, bind(listener.Get(), addr, length)) << errno;
        ASSERT_EQ(0, getsockname(listener.Get(), addr, &length)) << errno;
        ASSERT_EQ(0, listen(listener.Get(), 1)) << errno;
        client_.Reset(socket(GetParam(), SOCK_STREAM, 0));
        ASSERT_GE(client_.Get(), 0) << errno;
        // Bound blocking setup and receive operations even on a broken kernel.
        timeval timeout{5, 0};
        ASSERT_EQ(0, setsockopt(client_.Get(), SOL_SOCKET, SO_SNDTIMEO,
                               &timeout, sizeof(timeout))) << errno;
        ASSERT_EQ(0, setsockopt(client_.Get(), SOL_SOCKET, SO_RCVTIMEO,
                               &timeout, sizeof(timeout))) << errno;
        ASSERT_EQ(0, connect(client_.Get(), addr, length)) << errno;
        pollfd incoming{listener.Get(), POLLIN, 0};
        ASSERT_EQ(1, poll(&incoming, 1, 5000)) << errno;
        ASSERT_NE(0, incoming.revents & POLLIN);
        server_.Reset(accept(listener.Get(), nullptr, nullptr));
        ASSERT_GE(server_.Get(), 0) << errno;
    }

    void AbortPeer() {
        linger abort{1, 0};
        ASSERT_EQ(0, setsockopt(server_.Get(), SOL_SOCKET, SO_LINGER,
                               &abort, sizeof(abort))) << errno;
        server_.Reset();
        // POLLOUT is normally ready before the RST arrives. Request no normal
        // events here: POLLERR is unmaskable and does not consume SO_ERROR.
        ASSERT_NO_FATAL_FAILURE(WaitFor(0, POLLERR));
    }

    void WaitFor(short events, short wanted) {
        const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(5);
        for (;;) {
            auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(
                deadline - std::chrono::steady_clock::now()).count();
            ASSERT_GT(remaining, 0) << "timed out waiting for poll event " << wanted;
            pollfd pfd{client_.Get(), events, 0};
            int result = poll(&pfd, 1, static_cast<int>(remaining));
            if (result < 0 && errno == EINTR) continue;
            ASSERT_GE(result, 0) << errno;
            ASSERT_EQ(0, pfd.revents & POLLNVAL);
            if ((pfd.revents & wanted) == wanted) return;
        }
    }

    void ExpectError(int expected) {
        int error = -1;
        socklen_t length = sizeof(error);
        ASSERT_EQ(0, getsockopt(client_.Get(), SOL_SOCKET, SO_ERROR, &error, &length))
            << errno;
        EXPECT_EQ(sizeof(error), length);
        EXPECT_EQ(expected, error);
    }

    void ExpectResetPoll(bool pending) {
        pollfd pfd{client_.Get(), kReady, 0};
        ASSERT_EQ(1, poll(&pfd, 1, 0)) << errno;
        EXPECT_EQ(kReady | POLLHUP | (pending ? POLLERR : 0), pfd.revents);
    }

    Fd client_;
    Fd server_;
};

TEST_P(TcpPendingError, SoErrorConsumesResetExactlyOnce) {
    ASSERT_NO_FATAL_FAILURE(AbortPeer());
    ASSERT_NO_FATAL_FAILURE(ExpectResetPoll(true));
    ASSERT_NO_FATAL_FAILURE(ExpectResetPoll(true));
    ASSERT_NO_FATAL_FAILURE(ExpectError(ECONNRESET));
    ASSERT_NO_FATAL_FAILURE(ExpectError(0));
    ASSERT_NO_FATAL_FAILURE(ExpectResetPoll(false));
    char byte;
    EXPECT_EQ(0, recv(client_.Get(), &byte, 1, MSG_DONTWAIT));
    EXPECT_EQ(-1, send(client_.Get(), "x", 1, MSG_NOSIGNAL | MSG_DONTWAIT));
    EXPECT_EQ(EPIPE, errno);
}

TEST_P(TcpPendingError, ReceiveConsumesResetThenReturnsEof) {
    ASSERT_NO_FATAL_FAILURE(AbortPeer());
    char byte;
    ASSERT_EQ(-1, recv(client_.Get(), &byte, 1, MSG_DONTWAIT));
    EXPECT_EQ(ECONNRESET, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectError(0));
    EXPECT_EQ(0, recv(client_.Get(), &byte, 1, MSG_DONTWAIT));
    ASSERT_NO_FATAL_FAILURE(ExpectResetPoll(false));
}

TEST_P(TcpPendingError, SendConsumesResetThenReturnsBrokenPipe) {
    ASSERT_NO_FATAL_FAILURE(AbortPeer());
    ASSERT_EQ(-1, send(client_.Get(), "x", 1, MSG_NOSIGNAL | MSG_DONTWAIT));
    EXPECT_EQ(ECONNRESET, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectError(0));
    ASSERT_EQ(-1, send(client_.Get(), "x", 1, MSG_NOSIGNAL | MSG_DONTWAIT));
    EXPECT_EQ(EPIPE, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectResetPoll(false));
}

TEST_P(TcpPendingError, UncorkAndPollPreservePendingReset) {
    int cork = 1;
    ASSERT_EQ(0, setsockopt(client_.Get(), IPPROTO_TCP, TCP_CORK,
                           &cork, sizeof(cork))) << errno;
    ASSERT_EQ(1, send(client_.Get(), "x", 1, MSG_NOSIGNAL | MSG_DONTWAIT));
    ASSERT_NO_FATAL_FAILURE(AbortPeer());
    // Explicit uncork exercises queued output without depending on the cork
    // timer. Neither output flushing nor readiness queries may consume sk_err.
    cork = 0;
    ASSERT_EQ(0, setsockopt(client_.Get(), IPPROTO_TCP, TCP_CORK,
                           &cork, sizeof(cork))) << errno;
    for (int i = 0; i < 3; ++i) {
        ASSERT_NO_FATAL_FAILURE(ExpectResetPoll(true));
    }
    ASSERT_NO_FATAL_FAILURE(ExpectError(ECONNRESET));
    ASSERT_NO_FATAL_FAILURE(ExpectError(0));
    ASSERT_NO_FATAL_FAILURE(ExpectResetPoll(false));
}

TEST_P(TcpPendingError, CorkCannotQueueDataAfterResetWasConsumed) {
    int cork = 1;
    ASSERT_EQ(0, setsockopt(client_.Get(), IPPROTO_TCP, TCP_CORK,
                           &cork, sizeof(cork))) << errno;
    ASSERT_NO_FATAL_FAILURE(AbortPeer());
    ASSERT_NO_FATAL_FAILURE(ExpectError(ECONNRESET));
    ASSERT_EQ(-1, send(client_.Get(), "x", 1, MSG_NOSIGNAL | MSG_DONTWAIT));
    EXPECT_EQ(EPIPE, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectError(0));
    ASSERT_NO_FATAL_FAILURE(ExpectResetPoll(false));
}

TEST_P(TcpPendingError, PartialBlockingSendPreservesReset) {
    int size = 4096;
    ASSERT_EQ(0, setsockopt(client_.Get(), SOL_SOCKET, SO_SNDBUF,
                           &size, sizeof(size))) << errno;
    ASSERT_EQ(0, setsockopt(server_.Get(), SOL_SOCKET, SO_RCVBUF,
                           &size, sizeof(size))) << errno;
    timeval timeout{5, 0};
    ASSERT_EQ(0, setsockopt(server_.Get(), SOL_SOCKET, SO_RCVTIMEO,
                           &timeout, sizeof(timeout))) << errno;
    linger abort{1, 0};
    ASSERT_EQ(0, setsockopt(server_.Get(), SOL_SOCKET, SO_LINGER,
                           &abort, sizeof(abort))) << errno;
    // Much larger than both socket buffers, so send cannot complete before
    // the receiver aborts. Reading one byte proves send has made progress.
    std::vector<char> payload(8 * 1024 * 1024, 'x');
    ssize_t sent = -1;
    int send_errno = 0;
    std::thread sender([&] {
        sent = send(client_.Get(), payload.data(), payload.size(), MSG_NOSIGNAL);
        send_errno = errno;
    });
    char byte;
    ssize_t received = recv(server_.Get(), &byte, 1, 0);
    int receive_errno = errno;
    server_.Reset();
    // Join before assertions so even a receive failure cleans up the worker.
    sender.join();
    ASSERT_EQ(1, received) << receive_errno;
    ASSERT_GT(sent, 0) << send_errno;
    ASSERT_LT(static_cast<size_t>(sent), payload.size());
    ASSERT_NO_FATAL_FAILURE(WaitFor(0, POLLERR));
    ASSERT_NO_FATAL_FAILURE(ExpectResetPoll(true));
    ASSERT_NO_FATAL_FAILURE(ExpectError(ECONNRESET));
    ASSERT_NO_FATAL_FAILURE(ExpectError(0));
    ASSERT_NO_FATAL_FAILURE(ExpectResetPoll(false));
}

TEST_P(TcpPendingError, QueuedDataPrecedesReset) {
    ASSERT_EQ(1, send(server_.Get(), "x", 1, MSG_NOSIGNAL));
    char byte = 0;
    // PEEK proves the byte has reached the receiving queue without consuming it.
    ASSERT_EQ(1, recv(client_.Get(), &byte, 1, MSG_PEEK));
    ASSERT_NO_FATAL_FAILURE(AbortPeer());
    ASSERT_EQ(1, recv(client_.Get(), &byte, 1, MSG_DONTWAIT));
    EXPECT_EQ('x', byte);
    ASSERT_NO_FATAL_FAILURE(ExpectResetPoll(true));
    ASSERT_EQ(-1, recv(client_.Get(), &byte, 1, MSG_DONTWAIT));
    EXPECT_EQ(ECONNRESET, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectError(0));
    EXPECT_EQ(0, recv(client_.Get(), &byte, 1, MSG_DONTWAIT));
}

TEST_P(TcpPendingError, GracefulFinDoesNotSetError) {
    ASSERT_EQ(0, shutdown(server_.Get(), SHUT_WR));
    ASSERT_NO_FATAL_FAILURE(WaitFor(POLLRDHUP, POLLRDHUP));
    pollfd pfd{client_.Get(), kReady, 0};
    ASSERT_EQ(1, poll(&pfd, 1, 0));
    EXPECT_EQ(kReady, pfd.revents);
    ASSERT_NO_FATAL_FAILURE(ExpectError(0));
    char byte;
    EXPECT_EQ(0, recv(client_.Get(), &byte, 1, MSG_DONTWAIT));
    EXPECT_EQ(1, send(client_.Get(), "x", 1, MSG_NOSIGNAL));
}

TEST_P(TcpPendingError, ResetAfterFinReportsBrokenPipe) {
    ASSERT_EQ(0, shutdown(server_.Get(), SHUT_WR));
    ASSERT_NO_FATAL_FAILURE(WaitFor(POLLRDHUP, POLLRDHUP));
    ASSERT_NO_FATAL_FAILURE(ExpectError(0));
    ASSERT_NO_FATAL_FAILURE(AbortPeer());
    ASSERT_NO_FATAL_FAILURE(ExpectResetPoll(true));
    char byte;
    // tcp_recvmsg checks SOCK_DONE before sk_err after FIN; EOF preserves EPIPE.
    EXPECT_EQ(0, recv(client_.Get(), &byte, 1, MSG_DONTWAIT));
    ASSERT_NO_FATAL_FAILURE(ExpectResetPoll(true));
    ASSERT_NO_FATAL_FAILURE(ExpectError(EPIPE));
    ASSERT_NO_FATAL_FAILURE(ExpectError(0));
    ASSERT_NO_FATAL_FAILURE(ExpectResetPoll(false));
}

INSTANTIATE_TEST_SUITE_P(Loopback, TcpPendingError, ::testing::Values(AF_INET, AF_INET6),
                        [](const ::testing::TestParamInfo<int>& info) {
                            return info.param == AF_INET ? "IPv4" : "IPv6";
                        });

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
