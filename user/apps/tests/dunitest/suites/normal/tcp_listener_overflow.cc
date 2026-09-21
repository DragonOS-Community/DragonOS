#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <cerrno>
#include <chrono>
#include <cstring>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>
#include <vector>

namespace {
using Clock = std::chrono::steady_clock;

class TcpListenerOverflow : public testing::TestWithParam<int> {
 protected:
    std::vector<int> owned;
    int listener = -1;
    sockaddr_storage address{};
    socklen_t length = 0;

    int Socket(int family) {
        int fd = socket(family, SOCK_STREAM | SOCK_NONBLOCK, 0);
        if (fd >= 0) owned.push_back(fd);
        return fd;
    }
    void SetUp() override {
        bool ipv6 = GetParam() & 1;
        listener = Socket(ipv6 ? AF_INET6 : AF_INET);
        ASSERT_GE(listener, 0);
        if (ipv6) {
            int one = 1;
            ASSERT_EQ(0, setsockopt(listener, IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof(one)));
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
        if (ipv6)
            reinterpret_cast<sockaddr_in6*>(&address)->sin6_addr = in6addr_loopback;
        else
            reinterpret_cast<sockaddr_in*>(&address)->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    }
    void TearDown() override {
        for (int fd : owned) if (fd >= 0) close(fd);
    }
    void ExpectRefused(const sockaddr_storage& target, socklen_t size) {
        int fd = Socket(target.ss_family);
        ASSERT_GE(fd, 0);
        int result = connect(fd, reinterpret_cast<const sockaddr*>(&target), size);
        if (result < 0 && errno == ECONNREFUSED) return;
        ASSERT_EQ(-1, result);
        ASSERT_EQ(EINPROGRESS, errno);
        pollfd p{fd, POLLOUT, 0};
        ASSERT_EQ(1, poll(&p, 1, 2000));
        int error = 0;
        socklen_t n = sizeof(error);
        ASSERT_EQ(0, getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &n));
        ASSERT_EQ(ECONNREFUSED, error);
    }
    void Recover(int backlog, int count) {
        ASSERT_EQ(0, listen(listener, backlog));
        std::vector<int> clients;
        std::vector<bool> connected(count, false), sent(count, false), echoed(count, false);
        for (int i = 0; i < count; ++i) {
            int fd = Socket(address.ss_family);
            ASSERT_GE(fd, 0);
            clients.push_back(fd);
            int result = connect(fd, reinterpret_cast<sockaddr*>(&address), length);
            ASSERT_TRUE(result == 0 || (result < 0 && errno == EINPROGRESS)) << strerror(errno);
            connected[i] = result == 0;
        }
        // Do not accept yet: exercise exhaustion without assuming Linux and
        // DragonOS have identical physical slots or accept-queue accounting.
        auto hold = Clock::now() + std::chrono::milliseconds(150);
        while (Clock::now() < hold) {
            std::vector<pollfd> pending;
            for (int i = 0; i < count; ++i)
                pending.push_back({connected[i] ? -1 : clients[i], POLLOUT, 0});
            ASSERT_GE(poll(pending.data(), pending.size(), 20), 0);
            for (int i = 0; i < count; ++i) if (pending[i].revents) {
                int error = -1;
                socklen_t n = sizeof(error);
                ASSERT_EQ(0, getsockopt(clients[i], SOL_SOCKET, SO_ERROR, &error, &n));
                ASSERT_EQ(0, error) << "live saturated listener must not reset new SYN";
                connected[i] = true;
            }
        }

        std::vector<int> accepted;
        std::vector<bool> replied;
        auto deadline = Clock::now() + std::chrono::seconds(7);
        int completed = 0;
        while (completed < count && Clock::now() < deadline) {
            // A single deadline bounds the entire burst, not each connection.
            std::vector<pollfd> events{{listener, POLLIN, 0}};
            for (int i = 0; i < count; ++i)
                events.push_back({echoed[i] ? -1 : clients[i],
                                  static_cast<short>(sent[i] ? POLLIN : POLLOUT), 0});
            for (size_t i = 0; i < accepted.size(); ++i)
                events.push_back({replied[i] ? -1 : accepted[i], POLLIN, 0});
            ASSERT_GE(poll(events.data(), events.size(), 50), 0);
            for (int i = 0; i < count; ++i) if (events[i + 1].revents) {
                if (!sent[i]) {
                    int error = -1;
                    socklen_t n = sizeof(error);
                    ASSERT_EQ(0, getsockopt(clients[i], SOL_SOCKET, SO_ERROR, &error, &n));
                    ASSERT_EQ(0, error);
                    ASSERT_EQ(1, send(clients[i], "x", 1, MSG_NOSIGNAL));
                    sent[i] = true;
                } else {
                    char byte = 0;
                    ASSERT_EQ(1, recv(clients[i], &byte, 1, 0));
                    ASSERT_EQ('y', byte);
                    echoed[i] = true;
                    ++completed;
                }
            }
            for (size_t i = 0; i < accepted.size(); ++i)
                if (events[1 + count + i].revents) {
                    char byte = 0;
                    ASSERT_EQ(1, recv(accepted[i], &byte, 1, 0));
                    ASSERT_EQ('x', byte);
                    ASSERT_EQ(1, send(accepted[i], "y", 1, MSG_NOSIGNAL));
                    replied[i] = true;
                }
            while (accepted.size() < static_cast<size_t>(count)) {
                int fd = accept4(listener, nullptr, nullptr, SOCK_NONBLOCK);
                if (fd < 0) {
                    ASSERT_EQ(EAGAIN, errno);
                    break;
                }
                owned.push_back(fd);
                accepted.push_back(fd);
                replied.push_back(false);
            }
        }
        ASSERT_EQ(count, completed) << "TCP retransmission did not recover the burst";
        ASSERT_EQ(static_cast<size_t>(count), accepted.size());
        // New socket => new four-tuple; existing established peers cannot mask
        // stale listener registration after close.
        ASSERT_EQ(0, close(listener));
        for (int& fd : owned) if (fd == listener) fd = -1;
        ASSERT_NO_FATAL_FAILURE(ExpectRefused(address, length));
    }
};

TEST_P(TcpListenerOverflow, BurstRecoversAfterAccept) { Recover(8, 16); }
TEST_P(TcpListenerOverflow, ZeroBacklogRecoversAfterAccept) { Recover(0, 2); }

TEST_P(TcpListenerOverflow, OtherAddressFamilyRemainsRefused) {
    ASSERT_EQ(0, listen(listener, 8));
    sockaddr_storage target{};
    if (address.ss_family == AF_INET6) {
        auto* a = reinterpret_cast<sockaddr_in*>(&target);
        a->sin_family = AF_INET;
        a->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        a->sin_port = reinterpret_cast<sockaddr_in6*>(&address)->sin6_port;
        ExpectRefused(target, sizeof(*a));
    } else {
        auto* a = reinterpret_cast<sockaddr_in6*>(&target);
        a->sin6_family = AF_INET6;
        a->sin6_addr = in6addr_loopback;
        a->sin6_port = reinterpret_cast<sockaddr_in*>(&address)->sin_port;
        ExpectRefused(target, sizeof(*a));
    }
}

std::string DomainName(const testing::TestParamInfo<int>& info) {
    const char* names[] = {"IPv4", "IPv6", "Any4", "Any6"};
    return names[info.param];
}
INSTANTIATE_TEST_SUITE_P(AddressDomains, TcpListenerOverflow, testing::Values(0, 1, 2, 3), DomainName);
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
