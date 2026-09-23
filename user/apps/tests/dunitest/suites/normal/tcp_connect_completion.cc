// Successful nonblocking connect must not require a first client send before
// peer data/EOF becomes visible to poll and recv.
#include <gtest/gtest.h>
#include <arpa/inet.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>

#include <cerrno>
#include <cstring>

namespace {
class Fd {
 public:
    ~Fd() { if (fd_ >= 0) close(fd_); }
    Fd() = default;
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return fd_; }
    void Set(int fd) { fd_ = fd; }
 private:
    int fd_ = -1;
};

class TcpConnectCompletion : public testing::TestWithParam<int> {
 protected:
    Fd listener_, client_, accepted_;

    void Wait(int fd, short requested, short* returned = nullptr) {
        pollfd event{fd, requested, 0};
        ASSERT_EQ(poll(&event, 1, 3000), 1) << strerror(errno);
        ASSERT_NE(event.revents & requested, 0);
        if (returned) *returned = event.revents;
    }
    void SetUp() override {
        int family = GetParam();
        listener_.Set(socket(family, SOCK_STREAM | SOCK_NONBLOCK, 0));
        client_.Set(socket(family, SOCK_STREAM | SOCK_NONBLOCK, 0));
        ASSERT_GE(listener_.get(), 0); ASSERT_GE(client_.get(), 0);
        sockaddr_storage address{};
        socklen_t size;
        if (family == AF_INET) {
            auto* local = reinterpret_cast<sockaddr_in*>(&address);
            local->sin_family = family; local->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
            size = sizeof(*local);
        } else {
            auto* local = reinterpret_cast<sockaddr_in6*>(&address);
            local->sin6_family = family; local->sin6_addr = in6addr_loopback;
            size = sizeof(*local);
        }
        ASSERT_EQ(bind(listener_.get(), reinterpret_cast<sockaddr*>(&address), size), 0);
        ASSERT_EQ(getsockname(listener_.get(), reinterpret_cast<sockaddr*>(&address), &size), 0);
        ASSERT_EQ(listen(listener_.get(), 1), 0);
        int result = connect(client_.get(), reinterpret_cast<sockaddr*>(&address), size);
        if (result < 0) { ASSERT_EQ(errno, EINPROGRESS) << strerror(errno); }
        ASSERT_NO_FATAL_FAILURE(Wait(client_.get(), POLLOUT));
        int error = -1; socklen_t error_size = sizeof(error);
        ASSERT_EQ(getsockopt(client_.get(), SOL_SOCKET, SO_ERROR, &error, &error_size), 0);
        ASSERT_EQ(error, 0) << strerror(error);
        ASSERT_NO_FATAL_FAILURE(Wait(listener_.get(), POLLIN));
        accepted_.Set(accept4(listener_.get(), nullptr, nullptr, SOCK_NONBLOCK));
        ASSERT_GE(accepted_.get(), 0) << strerror(errno);
        // Do not send, recv, repeat connect, or shutdown on the client here:
        // any of these could conceal a missing connection-completion handoff.
    }
};

TEST_P(TcpConnectCompletion, FirstPeerDataIsReadableWithoutClientSend) {
    constexpr char payload[] = "first peer bytes";
    ASSERT_EQ(send(accepted_.get(), payload, sizeof(payload), MSG_NOSIGNAL),
              static_cast<ssize_t>(sizeof(payload)));
    ASSERT_NO_FATAL_FAILURE(Wait(client_.get(), POLLIN));
    char received[sizeof(payload)]{};
    size_t total = 0;
    while (total < sizeof(received)) {
        ASSERT_NO_FATAL_FAILURE(Wait(client_.get(), POLLIN));
        ssize_t count = recv(client_.get(), received + total, sizeof(received) - total, 0);
        ASSERT_GT(count, 0) << strerror(errno);
        total += count;
    }
    EXPECT_EQ(memcmp(received, payload, sizeof(payload)), 0);
}

TEST_P(TcpConnectCompletion, FirstPeerFinIsReadableWithoutClientSend) {
    ASSERT_EQ(shutdown(accepted_.get(), SHUT_WR), 0);
    short events = 0;
    ASSERT_NO_FATAL_FAILURE(Wait(client_.get(), POLLIN | POLLRDHUP, &events));
    EXPECT_NE(events & POLLIN, 0);
    EXPECT_NE(events & POLLRDHUP, 0);
    EXPECT_EQ(events & (POLLERR | POLLHUP), 0) << "peer half-close is not full hangup";
    char byte;
    ASSERT_EQ(recv(client_.get(), &byte, 1, 0), 0) << strerror(errno);
    // EOF is level-triggered, including after the application consumes it.
    ASSERT_NO_FATAL_FAILURE(Wait(client_.get(), POLLIN));
    ASSERT_EQ(recv(client_.get(), &byte, 1, 0), 0);
}

INSTANTIATE_TEST_SUITE_P(IpFamilies, TcpConnectCompletion,
                        testing::Values(AF_INET, AF_INET6),
                        [](const testing::TestParamInfo<int>& info) {
                            return info.param == AF_INET ? "IPv4" : "IPv6";
                        });
}  // namespace
