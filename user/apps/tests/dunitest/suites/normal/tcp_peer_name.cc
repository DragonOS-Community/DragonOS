// Peer queries reflect the live TCP transport without consuming errors or
// acknowledging a completed nonblocking connect.
#include <gtest/gtest.h>
#include <arpa/inet.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>
#include <cerrno>
#include <chrono>
#include <cstring>
#include <thread>

namespace {
class Fd {
 public:
    explicit Fd(int fd = -1) : fd_(fd) {}
    ~Fd() { Reset(); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return fd_; }
    void Reset(int fd = -1) {
        if (fd_ >= 0) close(fd_);
        fd_ = fd;
    }
 private:
    int fd_;
};

class TcpPeerName : public testing::TestWithParam<int> {
 protected:
    Fd listener_, client_, peer_;
    sockaddr_storage destination_{};
    socklen_t size_ = 0;

    void SetUp() override {
        listener_.Reset(socket(GetParam(), SOCK_STREAM | SOCK_NONBLOCK, 0));
        client_.Reset(socket(GetParam(), SOCK_STREAM | SOCK_NONBLOCK, 0));
        ASSERT_GE(listener_.get(), 0); ASSERT_GE(client_.get(), 0);
        if (GetParam() == AF_INET) {
            auto* address = reinterpret_cast<sockaddr_in*>(&destination_);
            address->sin_family = AF_INET;
            address->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
            size_ = sizeof(*address);
        } else {
            auto* address = reinterpret_cast<sockaddr_in6*>(&destination_);
            address->sin6_family = AF_INET6;
            address->sin6_addr = in6addr_loopback;
            size_ = sizeof(*address);
        }
        ASSERT_EQ(bind(listener_.get(), Address(), size_), 0) << errno;
        ASSERT_EQ(getsockname(listener_.get(), Address(), &size_), 0);
    }
    sockaddr* Address() { return reinterpret_cast<sockaddr*>(&destination_); }
    void Wait(int fd, short events, short wanted) {
        pollfd ready{fd, events, 0};
        ASSERT_EQ(poll(&ready, 1, 3000), 1) << errno;
        ASSERT_NE(ready.revents & wanted, 0) << ready.revents;
    }
    void NoPeer(int fd) {
        sockaddr_storage actual{};
        socklen_t size = sizeof(actual);
        ASSERT_EQ(getpeername(fd, reinterpret_cast<sockaddr*>(&actual), &size), -1);
        ASSERT_EQ(errno, ENOTCONN);
    }
    void PeerIs(int fd, const sockaddr_storage& expected) {
        sockaddr_storage actual{};
        socklen_t size = sizeof(actual);
        ASSERT_EQ(getpeername(fd, reinterpret_cast<sockaddr*>(&actual), &size), 0) << errno;
        ASSERT_EQ(actual.ss_family, expected.ss_family);
        if (GetParam() == AF_INET) {
            auto* a = reinterpret_cast<sockaddr_in*>(&actual);
            auto* e = reinterpret_cast<const sockaddr_in*>(&expected);
            EXPECT_EQ(a->sin_port, e->sin_port);
            EXPECT_EQ(a->sin_addr.s_addr, e->sin_addr.s_addr);
            EXPECT_EQ(size, sizeof(*a));
        } else {
            auto* a = reinterpret_cast<sockaddr_in6*>(&actual);
            auto* e = reinterpret_cast<const sockaddr_in6*>(&expected);
            EXPECT_EQ(a->sin6_port, e->sin6_port);
            EXPECT_EQ(memcmp(&a->sin6_addr, &e->sin6_addr, sizeof(a->sin6_addr)), 0);
            EXPECT_EQ(a->sin6_scope_id, e->sin6_scope_id);
            EXPECT_EQ(size, sizeof(*a));
        }
    }
    void ErrorIs(int expected) {
        int error = -1; socklen_t size = sizeof(error);
        ASSERT_EQ(getsockopt(client_.get(), SOL_SOCKET, SO_ERROR, &error, &size), 0);
        ASSERT_EQ(error, expected);
    }
    int Connect() { return connect(client_.get(), Address(), size_); }
    void Open() {
        ASSERT_EQ(listen(listener_.get(), 2), 0);
        ASSERT_EQ(Connect(), -1); ASSERT_EQ(errno, EINPROGRESS);
        ASSERT_NO_FATAL_FAILURE(Wait(listener_.get(), POLLIN, POLLIN));
        peer_.Reset(accept4(listener_.get(), nullptr, nullptr, SOCK_NONBLOCK));
        ASSERT_GE(peer_.get(), 0);
        ASSERT_NO_FATAL_FAILURE(Wait(client_.get(), POLLOUT, POLLOUT));
    }
    void ResetPeer() {
        linger reset{1, 0};
        ASSERT_EQ(setsockopt(peer_.get(), SOL_SOCKET, SO_LINGER, &reset, sizeof(reset)), 0);
        peer_.Reset();
        ASSERT_NO_FATAL_FAILURE(Wait(client_.get(), 0, POLLERR));
    }
};

TEST_P(TcpPeerName, FreshBoundAndListeningHaveNoPeer) {
    ASSERT_NO_FATAL_FAILURE(NoPeer(client_.get()));
    ASSERT_NO_FATAL_FAILURE(NoPeer(listener_.get()));
    ASSERT_EQ(listen(listener_.get(), 1), 0);
    ASSERT_NO_FATAL_FAILURE(NoPeer(listener_.get()));
}

TEST_P(TcpPeerName, PendingSynDoesNotExposeDestination) {
    ASSERT_EQ(listen(listener_.get(), 0), 0);
    Fd blocker(socket(GetParam(), SOCK_STREAM, 0));
    ASSERT_GE(blocker.get(), 0);
    ASSERT_EQ(connect(blocker.get(), Address(), size_), 0);
    ASSERT_EQ(Connect(), -1); ASSERT_EQ(errno, EINPROGRESS);
    pollfd pending{client_.get(), POLLOUT, 0};
    ASSERT_EQ(poll(&pending, 1, 20), 0) << "backlog must hold SYN_SENT";
    ASSERT_NO_FATAL_FAILURE(NoPeer(client_.get()));
    // A public peer query and the route's internal destination are separate:
    // the SYN has a cached output path even though getpeername is ENOTCONN.
    int mtu = 0; socklen_t size = sizeof(mtu);
    ASSERT_EQ(getsockopt(client_.get(), GetParam() == AF_INET ? IPPROTO_IP : IPPROTO_IPV6,
                         GetParam() == AF_INET ? IP_MTU : IPV6_MTU, &mtu, &size), 0) << errno;
    EXPECT_GT(mtu, 0);
    ASSERT_NO_FATAL_FAILURE(ErrorIs(0));
    ASSERT_EQ(Connect(), -1); EXPECT_EQ(errno, EALREADY);
}

TEST_P(TcpPeerName, CompletedHandshakeQueryDoesNotConfirmConnect) {
    ASSERT_NO_FATAL_FAILURE(Open());
    ASSERT_NO_FATAL_FAILURE(PeerIs(client_.get(), destination_));
    ASSERT_NO_FATAL_FAILURE(PeerIs(client_.get(), destination_));
    sockaddr_storage source{}; socklen_t size = sizeof(source);
    ASSERT_EQ(getsockname(client_.get(), reinterpret_cast<sockaddr*>(&source), &size), 0);
    ASSERT_NO_FATAL_FAILURE(PeerIs(peer_.get(), source));
    ASSERT_EQ(Connect(), 0) << errno;
    ASSERT_EQ(Connect(), -1); EXPECT_EQ(errno, EISCONN);
}

TEST_P(TcpPeerName, RefusedHandshakeQueryDoesNotConsumeError) {
    // Bound but not listening: keep the port reserved while provoking RST.
    ASSERT_EQ(Connect(), -1); ASSERT_EQ(errno, EINPROGRESS);
    ASSERT_NO_FATAL_FAILURE(Wait(client_.get(), 0, POLLERR));
    ASSERT_NO_FATAL_FAILURE(NoPeer(client_.get()));
    ASSERT_NO_FATAL_FAILURE(NoPeer(client_.get()));
    ASSERT_NO_FATAL_FAILURE(ErrorIs(ECONNREFUSED));
    ASSERT_NO_FATAL_FAILURE(NoPeer(client_.get()));
    ASSERT_NO_FATAL_FAILURE(ErrorIs(0));
    ASSERT_EQ(Connect(), -1); ASSERT_EQ(errno, ECONNABORTED);
    ASSERT_NO_FATAL_FAILURE(NoPeer(client_.get()));
    ASSERT_EQ(listen(listener_.get(), 1), 0);
    ASSERT_EQ(Connect(), -1); ASSERT_EQ(errno, EINPROGRESS);
    ASSERT_NO_FATAL_FAILURE(Wait(listener_.get(), POLLIN, POLLIN));
    peer_.Reset(accept4(listener_.get(), nullptr, nullptr, SOCK_NONBLOCK));
    ASSERT_GE(peer_.get(), 0);
    ASSERT_NO_FATAL_FAILURE(Wait(client_.get(), POLLOUT, POLLOUT));
    ASSERT_NO_FATAL_FAILURE(PeerIs(client_.get(), destination_));
}

TEST_P(TcpPeerName, ResetQueryDoesNotConsumeSocketError) {
    ASSERT_NO_FATAL_FAILURE(Open());
    ASSERT_NO_FATAL_FAILURE(ResetPeer());
    ASSERT_NO_FATAL_FAILURE(NoPeer(client_.get()));
    ASSERT_NO_FATAL_FAILURE(NoPeer(client_.get()));
    ASSERT_NO_FATAL_FAILURE(ErrorIs(ECONNRESET));
    ASSERT_NO_FATAL_FAILURE(NoPeer(client_.get()));
    ASSERT_NO_FATAL_FAILURE(ErrorIs(0));
}

TEST_P(TcpPeerName, ResetQueryDoesNotConsumeReceiveError) {
    ASSERT_NO_FATAL_FAILURE(Open());
    ASSERT_NO_FATAL_FAILURE(ResetPeer());
    ASSERT_NO_FATAL_FAILURE(NoPeer(client_.get()));
    char byte;
    ASSERT_EQ(recv(client_.get(), &byte, 1, MSG_DONTWAIT), -1);
    ASSERT_EQ(errno, ECONNRESET);
    ASSERT_NO_FATAL_FAILURE(NoPeer(client_.get()));
    ASSERT_NO_FATAL_FAILURE(ErrorIs(0));
}

TEST_P(TcpPeerName, ResetAfterConnectConfirmationHasNoPeer) {
    ASSERT_NO_FATAL_FAILURE(Open());
    ASSERT_EQ(Connect(), 0);
    ASSERT_EQ(Connect(), -1); ASSERT_EQ(errno, EISCONN);
    ASSERT_NO_FATAL_FAILURE(PeerIs(client_.get(), destination_));
    ASSERT_NO_FATAL_FAILURE(ResetPeer());
    ASSERT_NO_FATAL_FAILURE(NoPeer(client_.get()));
    ASSERT_NO_FATAL_FAILURE(ErrorIs(ECONNRESET));
    ASSERT_NO_FATAL_FAILURE(NoPeer(client_.get()));
}

TEST_P(TcpPeerName, ResetPeerQueryPreservesBufferedDataAndReset) {
    ASSERT_NO_FATAL_FAILURE(Open());
    const char payload[] = "before reset";
    ASSERT_EQ(send(peer_.get(), payload, sizeof(payload), MSG_NOSIGNAL),
              static_cast<ssize_t>(sizeof(payload)));
    ASSERT_NO_FATAL_FAILURE(Wait(client_.get(), POLLIN, POLLIN));
    ASSERT_NO_FATAL_FAILURE(ResetPeer());
    ASSERT_NO_FATAL_FAILURE(NoPeer(client_.get()));
    char received[sizeof(payload)]{};
    ASSERT_EQ(recv(client_.get(), received, sizeof(received), MSG_DONTWAIT),
              static_cast<ssize_t>(sizeof(received)));
    EXPECT_EQ(memcmp(received, payload, sizeof(payload)), 0);
    ASSERT_NO_FATAL_FAILURE(NoPeer(client_.get()));
    ASSERT_EQ(recv(client_.get(), received, sizeof(received), MSG_DONTWAIT), -1);
    ASSERT_EQ(errno, ECONNRESET);
    ASSERT_NO_FATAL_FAILURE(NoPeer(client_.get()));
    ASSERT_NO_FATAL_FAILURE(ErrorIs(0));
}

TEST_P(TcpPeerName, ReadShutdownPreservesPeer) {
    ASSERT_NO_FATAL_FAILURE(Open());
    ASSERT_EQ(shutdown(client_.get(), SHUT_RD), 0);
    ASSERT_NO_FATAL_FAILURE(PeerIs(client_.get(), destination_));
}

TEST_P(TcpPeerName, FinWaitAndCloseWaitPreservePeerUntilFullClose) {
    ASSERT_NO_FATAL_FAILURE(Open());
    ASSERT_EQ(shutdown(client_.get(), SHUT_WR), 0);
    ASSERT_NO_FATAL_FAILURE(PeerIs(client_.get(), destination_));
    ASSERT_NO_FATAL_FAILURE(Wait(peer_.get(), POLLIN, POLLIN));
    char byte;
    ASSERT_EQ(recv(peer_.get(), &byte, 1, MSG_DONTWAIT), 0);
    sockaddr_storage source{}; socklen_t size = sizeof(source);
    ASSERT_EQ(getsockname(client_.get(), reinterpret_cast<sockaddr*>(&source), &size), 0);
    ASSERT_NO_FATAL_FAILURE(PeerIs(peer_.get(), source));
    ASSERT_EQ(shutdown(peer_.get(), SHUT_WR), 0);
    ASSERT_NO_FATAL_FAILURE(Wait(client_.get(), POLLIN, POLLIN));
    ASSERT_EQ(recv(client_.get(), &byte, 1, MSG_DONTWAIT), 0);
    // The full FIN exchange completes asynchronously. Linux moves the user's
    // socket to CLOSE while a separate timewait socket retains the tuple.
    const auto end = std::chrono::steady_clock::now() + std::chrono::seconds(3);
    for (;;) {
        sockaddr_storage actual{}; socklen_t length = sizeof(actual);
        if (getpeername(client_.get(), reinterpret_cast<sockaddr*>(&actual), &length) < 0) {
            ASSERT_EQ(errno, ENOTCONN); break;
        }
        ASSERT_LT(std::chrono::steady_clock::now(), end);
        std::this_thread::sleep_for(std::chrono::milliseconds(1));
    }
    ASSERT_NO_FATAL_FAILURE(NoPeer(client_.get()));
}

INSTANTIATE_TEST_SUITE_P(IpFamilies, TcpPeerName, testing::Values(AF_INET, AF_INET6));
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
