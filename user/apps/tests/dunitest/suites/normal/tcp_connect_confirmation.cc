// TCP handshake completion must not consume Linux's connect() confirmation.
#include <gtest/gtest.h>
#include <arpa/inet.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>
#include <cerrno>

namespace {
class TcpConnectConfirmation : public testing::TestWithParam<int> {
 protected:
    int listener_ = -1, client_ = -1, peer_ = -1;
    sockaddr_storage address_{};
    socklen_t length_ = 0;
    void TearDown() override {
        for (int fd : {client_, peer_, listener_}) if (fd >= 0) close(fd);
    }
    void Wait(int fd, short events) {
        pollfd p{fd, events, 0};
        ASSERT_EQ(poll(&p, 1, 3000), 1);
    }
    void Open(bool nonblocking = true) {
        const int family = GetParam();
        listener_ = socket(family, SOCK_STREAM, 0);
        ASSERT_GE(listener_, 0);
        if (family == AF_INET) {
            auto* a = reinterpret_cast<sockaddr_in*>(&address_);
            a->sin_family = family; a->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
            length_ = sizeof(*a);
        } else {
            auto* a = reinterpret_cast<sockaddr_in6*>(&address_);
            a->sin6_family = family; a->sin6_addr = in6addr_loopback;
            length_ = sizeof(*a);
        }
        ASSERT_EQ(bind(listener_, reinterpret_cast<sockaddr*>(&address_), length_), 0);
        ASSERT_EQ(getsockname(listener_, reinterpret_cast<sockaddr*>(&address_), &length_), 0);
        ASSERT_EQ(listen(listener_, 2), 0);
        client_ = socket(family, SOCK_STREAM | (nonblocking ? SOCK_NONBLOCK : 0), 0);
        ASSERT_GE(client_, 0);
        int result = Again();
        if (nonblocking) { ASSERT_EQ(result, -1); ASSERT_EQ(errno, EINPROGRESS); }
        else { ASSERT_EQ(result, 0); }
        ASSERT_NO_FATAL_FAILURE(Wait(listener_, POLLIN));
        peer_ = accept4(listener_, nullptr, nullptr, SOCK_NONBLOCK);
        ASSERT_GE(peer_, 0);
        ASSERT_NO_FATAL_FAILURE(Wait(client_, POLLOUT));
    }
    int Again() { return connect(client_, reinterpret_cast<sockaddr*>(&address_), length_); }
    void ConfirmOnce() {
        ASSERT_EQ(Again(), 0) << errno;
        ASSERT_EQ(Again(), -1); ASSERT_EQ(errno, EISCONN);
    }
};

TEST_P(TcpConnectConfirmation, PeerFinDoesNotConfirmConnect) {
    ASSERT_NO_FATAL_FAILURE(Open());
    ASSERT_EQ(shutdown(peer_, SHUT_WR), 0);
    ASSERT_NO_FATAL_FAILURE(Wait(client_, POLLIN));
    char byte;
    ASSERT_EQ(recv(client_, &byte, 1, 0), 0);
    ASSERT_NO_FATAL_FAILURE(ConfirmOnce());
}

TEST_P(TcpConnectConfirmation, DataAndSocketErrorQueryDoNotConfirmConnect) {
    ASSERT_NO_FATAL_FAILURE(Open());
    int error = -1; socklen_t size = sizeof(error);
    ASSERT_EQ(getsockopt(client_, SOL_SOCKET, SO_ERROR, &error, &size), 0);
    ASSERT_EQ(error, 0);
    char byte = 'c';
    ASSERT_EQ(send(client_, &byte, 1, MSG_NOSIGNAL), 1);
    ASSERT_NO_FATAL_FAILURE(Wait(peer_, POLLIN));
    ASSERT_EQ(recv(peer_, &byte, 1, 0), 1);
    ASSERT_EQ(send(peer_, &byte, 1, MSG_NOSIGNAL), 1);
    ASSERT_NO_FATAL_FAILURE(Wait(client_, POLLIN));
    ASSERT_EQ(recv(client_, &byte, 1, 0), 1);
    ASSERT_NO_FATAL_FAILURE(ConfirmOnce());
}

TEST_P(TcpConnectConfirmation, BlockingConnectAlreadyConfirmed) {
    ASSERT_NO_FATAL_FAILURE(Open(false));
    ASSERT_EQ(Again(), -1); ASSERT_EQ(errno, EISCONN);
}

TEST_P(TcpConnectConfirmation, ShutdownConfirmsConnect) {
    ASSERT_NO_FATAL_FAILURE(Open());
    ASSERT_EQ(shutdown(client_, SHUT_RD), 0);
    ASSERT_EQ(Again(), -1); ASSERT_EQ(errno, EISCONN);
}

TEST_P(TcpConnectConfirmation, AcceptedSocketAlreadyConfirmed) {
    ASSERT_NO_FATAL_FAILURE(Open());
    ASSERT_EQ(connect(peer_, reinterpret_cast<sockaddr*>(&address_), length_), -1);
    ASSERT_EQ(errno, EISCONN);
}

TEST_P(TcpConnectConfirmation, ResetBeforeConfirmationRestoresRetryableSocket) {
    ASSERT_NO_FATAL_FAILURE(Open());
    linger reset{1, 0};
    ASSERT_EQ(setsockopt(peer_, SOL_SOCKET, SO_LINGER, &reset, sizeof(reset)), 0);
    ASSERT_EQ(close(peer_), 0); peer_ = -1;
    ASSERT_NO_FATAL_FAILURE(Wait(client_, POLLIN));
    // Consume the reset through recv before connect: Linux then falls back to
    // ECONNABORTED, rather than reporting success or permanently EISCONN.
    char byte;
    ASSERT_EQ(recv(client_, &byte, 1, 0), -1); ASSERT_EQ(errno, ECONNRESET);
    ASSERT_EQ(Again(), -1); ASSERT_EQ(errno, ECONNABORTED);
    ASSERT_EQ(Again(), -1); ASSERT_EQ(errno, EINPROGRESS);
    ASSERT_NO_FATAL_FAILURE(Wait(listener_, POLLIN));
    peer_ = accept4(listener_, nullptr, nullptr, SOCK_NONBLOCK);
    ASSERT_GE(peer_, 0);
}

INSTANTIATE_TEST_SUITE_P(IpFamilies, TcpConnectConfirmation, testing::Values(AF_INET, AF_INET6));
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
