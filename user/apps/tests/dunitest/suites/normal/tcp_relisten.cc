#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <cerrno>
#include <climits>
#include <cstring>
#include <fcntl.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>
#include <vector>

namespace {
class TcpRelisten : public testing::TestWithParam<int> {
 protected:
    std::vector<int> fds;
    int listener = -1;
    sockaddr_storage address{};
    socklen_t length = 0;

    int Socket(bool nonblock = false) {
        int fd = socket(GetParam() & 1 ? AF_INET6 : AF_INET,
                        SOCK_STREAM | (nonblock ? SOCK_NONBLOCK : 0), 0);
        if (fd >= 0) fds.push_back(fd);
        return fd;
    }
    void SetUp() override {
        listener = Socket(true);
        ASSERT_GE(listener, 0);
        if (GetParam() & 1) {
            auto* a = reinterpret_cast<sockaddr_in6*>(&address);
            a->sin6_family = AF_INET6;
            a->sin6_addr = GetParam() & 2 ? in6addr_any : in6addr_loopback;
            length = sizeof(*a);
        } else {
            auto* a = reinterpret_cast<sockaddr_in*>(&address);
            a->sin_family = AF_INET;
            a->sin_addr.s_addr = htonl(GetParam() & 2 ? INADDR_ANY : INADDR_LOOPBACK);
            length = sizeof(*a);
        }
        ASSERT_EQ(0, bind(listener, reinterpret_cast<sockaddr*>(&address), length));
        ASSERT_EQ(0, getsockname(listener, reinterpret_cast<sockaddr*>(&address), &length));
        if (GetParam() & 1)
            reinterpret_cast<sockaddr_in6*>(&address)->sin6_addr = in6addr_loopback;
        else
            reinterpret_cast<sockaddr_in*>(&address)->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    }
    void TearDown() override {
        for (auto i = fds.rbegin(); i != fds.rend(); ++i) close(*i);
    }
    int Connect() {
        int fd = Socket(true);
        EXPECT_GE(fd, 0);
        int result = connect(fd, reinterpret_cast<sockaddr*>(&address), length);
        EXPECT_TRUE(result == 0 || errno == EINPROGRESS) << strerror(errno);
        pollfd p{fd, POLLOUT, 0};
        EXPECT_EQ(1, poll(&p, 1, 2000));
        int error = -1;
        socklen_t n = sizeof(error);
        EXPECT_EQ(0, getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &n));
        EXPECT_EQ(0, error);
        return fd;
    }
    void AcceptData() {
        pollfd p{listener, POLLIN, 0};
        ASSERT_EQ(1, poll(&p, 1, 2000));
        int accepted = accept4(listener, nullptr, nullptr, SOCK_NONBLOCK);
        ASSERT_GE(accepted, 0) << strerror(errno);
        fds.push_back(accepted);
        p = {accepted, POLLIN, 0};
        ASSERT_EQ(1, poll(&p, 1, 2000));
        char byte = 0;
        ASSERT_EQ(1, recv(accepted, &byte, 1, 0));
        EXPECT_EQ('x', byte);
    }
    void AcceptAndExchange(int peer) {
        ASSERT_EQ(1, send(peer, "x", 1, MSG_NOSIGNAL));
        AcceptData();
    }
    void Drain(const std::vector<int>& peers) {
        // Slot order need not match connection arrival order. Feed every peer
        // before accepting so the test checks preservation, not queue ordering.
        for (int peer : peers) ASSERT_EQ(1, send(peer, "x", 1, MSG_NOSIGNAL));
        for (size_t i = 0; i < peers.size(); ++i) AcceptData();
    }
};

TEST_P(TcpRelisten, RepeatedListenPreservesPendingConnection) {
    ASSERT_EQ(0, listen(listener, 4));
    int peer = Connect();
    for (int i = 0; i < 3; ++i) ASSERT_EQ(0, listen(listener, 4)) << strerror(errno);
    AcceptAndExchange(peer);
    AcceptAndExchange(Connect());
}

TEST_P(TcpRelisten, GrowFromZeroAdmitsMultipleConnections) {
    ASSERT_EQ(0, listen(listener, 0));
    ASSERT_EQ(0, listen(listener, 4)) << strerror(errno);
    std::vector<int> peers;
    for (int i = 0; i < 3; ++i) peers.push_back(Connect());
    Drain(peers);
}

TEST_P(TcpRelisten, ShrinkPreservesPendingConnections) {
    ASSERT_EQ(0, listen(listener, 4));
    std::vector<int> peers;
    for (int i = 0; i < 3; ++i) peers.push_back(Connect());
    ASSERT_EQ(0, listen(listener, 0)) << strerror(errno);
    Drain(peers);
    AcceptAndExchange(Connect());
}

TEST_P(TcpRelisten, ShrinkEmptyListenerToZero) {
    ASSERT_EQ(0, listen(listener, 4));
    ASSERT_EQ(0, listen(listener, 0)) << strerror(errno);
    int first = Connect();
    int extra = Socket(true);
    ASSERT_GE(extra, 0);
    ASSERT_EQ(-1, connect(extra, reinterpret_cast<sockaddr*>(&address), length));
    ASSERT_EQ(EINPROGRESS, errno);
    pollfd p{extra, POLLOUT, 0};
    EXPECT_EQ(0, poll(&p, 1, 150)) << "full zero backlog should not admit or reset SYN";
    AcceptAndExchange(first);
}

TEST_P(TcpRelisten, LargeAndNegativeBacklogAreClamped) {
    for (int backlog : {INT_MAX, -1, 65536, 0, 511})
        ASSERT_EQ(0, listen(listener, backlog)) << backlog << ": " << strerror(errno);
    AcceptAndExchange(Connect());
}

TEST_P(TcpRelisten, RepeatedZeroDoesNotAdmitAnotherConnection) {
    ASSERT_EQ(0, listen(listener, 0));
    int first = Connect();
    ASSERT_EQ(0, listen(listener, 0));
    int extra = Socket(true);
    ASSERT_GE(extra, 0);
    ASSERT_EQ(-1, connect(extra, reinterpret_cast<sockaddr*>(&address), length));
    ASSERT_EQ(EINPROGRESS, errno);
    pollfd p{extra, POLLOUT, 0};
    EXPECT_EQ(0, poll(&p, 1, 150));
    AcceptAndExchange(first);
}

TEST_P(TcpRelisten, GrowAgainAfterPendingShrink) {
    ASSERT_EQ(0, listen(listener, 4));
    std::vector<int> peers{Connect(), Connect(), Connect()};
    ASSERT_EQ(0, listen(listener, 0));
    Drain(peers);
    ASSERT_EQ(0, listen(listener, 4));
    peers = {Connect(), Connect(), Connect()};
    Drain(peers);
}

TEST_P(TcpRelisten, CancelPendingDuringShrink) {
    ASSERT_EQ(0, listen(listener, 4));
    int cancelled = Connect();
    int survivor = Connect();
    int cancelled2 = Connect();
    ASSERT_EQ(0, listen(listener, 0));
    for (int fd : {cancelled, cancelled2}) {
        linger reset{1, 0};
        ASSERT_EQ(0, setsockopt(fd, SOL_SOCKET, SO_LINGER, &reset, sizeof(reset)));
        ASSERT_EQ(0, close(fd));
        for (int& owned : fds) if (owned == fd) owned = -1;
    }
    ASSERT_EQ(1, send(survivor, "x", 1, MSG_NOSIGNAL));
    int delivered = 0;
    // Linux may still return a reset child from accept; DragonOS may already
    // have reaped its slot. Neither outcome may discard the surviving child.
    for (int i = 0; i < 3; ++i) {
        pollfd p{listener, POLLIN, 0};
        int ready = poll(&p, 1, i == 0 ? 2000 : 0);
        ASSERT_GE(ready, 0);
        if (ready == 0) break;
        int child = accept4(listener, nullptr, nullptr, SOCK_NONBLOCK);
        ASSERT_GE(child, 0) << strerror(errno);
        fds.push_back(child);
        p = {child, POLLIN, 0};
        ASSERT_EQ(1, poll(&p, 1, 2000));
        char byte = 0;
        int count = recv(child, &byte, 1, 0);
        if (count == 1) {
            EXPECT_EQ('x', byte);
            ++delivered;
        } else {
            EXPECT_TRUE(count == 0 || (count == -1 && errno == ECONNRESET));
        }
    }
    ASSERT_EQ(1, delivered);
    AcceptAndExchange(Connect());
}

TEST_P(TcpRelisten, ConnectedSocketStillRejectsListen) {
    ASSERT_EQ(0, listen(listener, 4));
    int peer = Connect();
    ASSERT_EQ(-1, listen(peer, 4));
    ASSERT_EQ(EINVAL, errno);
    AcceptAndExchange(peer);
}

TEST_P(TcpRelisten, ShrinkAfterLastPendingResetKeepsListening) {
    ASSERT_EQ(0, listen(listener, 4));
    // Leave one pending child and three idle slots, regardless of accept order.
    for (int i = 0; i < 3; ++i) AcceptAndExchange(Connect());
    int cancelled = Connect();
    linger reset{1, 0};
    ASSERT_EQ(0, setsockopt(cancelled, SOL_SOCKET, SO_LINGER, &reset, sizeof(reset)));
    ASSERT_EQ(0, close(cancelled));
    for (int& owned : fds) if (owned == cancelled) owned = -1;
    ASSERT_EQ(0, listen(listener, 0));
    // Drain any reset children retained by Linux; no data-bearing child may
    // disappear. DragonOS reuses a closed pending slot instead.
    for (int i = 0; i < 4; ++i) {
        int child = accept4(listener, nullptr, nullptr, SOCK_NONBLOCK);
        if (child < 0) {
            ASSERT_TRUE(errno == EAGAIN || errno == ECONNABORTED);
            break;
        }
        fds.push_back(child);
    }
    AcceptAndExchange(Connect());
}

std::string DomainName(const testing::TestParamInfo<int>& info) {
    const char* names[] = {"IPv4", "IPv6", "Any4", "Any6"};
    return names[info.param];
}
INSTANTIATE_TEST_SUITE_P(AddressDomains, TcpRelisten, testing::Values(0, 1, 2, 3), DomainName);
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
