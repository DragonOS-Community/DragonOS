// listen's logical capacity must not be silently reduced by an allocation cap.
#include <gtest/gtest.h>
#include <arpa/inet.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>

#include <cerrno>
#include <tuple>
#include <vector>

namespace {
class Descriptors {
 public:
    ~Descriptors() { for (int fd : values) close(fd); }
    int Add(int fd) { if (fd >= 0) values.push_back(fd); return fd; }
    std::vector<int> values;
};

class TcpListenBacklog : public testing::TestWithParam<std::tuple<int, int>> {};

TEST_P(TcpListenBacklog, AdmitsRequestedCapacityBeforeAnyAccept) {
    const auto [family, backlog] = GetParam();
    Descriptors descriptors;
    int listener = descriptors.Add(socket(family, SOCK_STREAM | SOCK_NONBLOCK, 0));
    ASSERT_GE(listener, 0);
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
    ASSERT_EQ(bind(listener, reinterpret_cast<sockaddr*>(&storage), size), 0);
    ASSERT_EQ(getsockname(listener, reinterpret_cast<sockaddr*>(&storage), &size), 0);
    ASSERT_EQ(listen(listener, backlog), 0);
    for (int index = 0; index <= backlog; ++index) {
        int client = descriptors.Add(socket(family, SOCK_STREAM | SOCK_NONBLOCK, 0));
        ASSERT_GE(client, 0);
        if (connect(client, reinterpret_cast<sockaddr*>(&storage), size) < 0) {
            ASSERT_EQ(errno, EINPROGRESS) << "connection " << index;
        }
        pollfd event{client, POLLOUT, 0};
        ASSERT_EQ(poll(&event, 1, 3000), 1) << "connection " << index;
        int error = -1;
        socklen_t length = sizeof(error);
        ASSERT_EQ(getsockopt(client, SOL_SOCKET, SO_ERROR, &error, &length), 0);
        ASSERT_EQ(error, 0) << "connection " << index;
        sockaddr_storage peer{};
        length = sizeof(peer);
        ASSERT_EQ(getpeername(client, reinterpret_cast<sockaddr*>(&peer), &length), 0);
    }
    // Do not infer server admission solely from the client SYN/SYN-ACK path:
    // every advertised slot must eventually yield a real accepted connection.
    for (int index = 0; index <= backlog; ++index) {
        pollfd event{listener, POLLIN, 0};
        ASSERT_EQ(poll(&event, 1, 3000), 1) << "accept " << index;
        ASSERT_GE(descriptors.Add(accept4(listener, nullptr, nullptr, SOCK_NONBLOCK)), 0);
    }
    EXPECT_EQ(descriptors.Add(accept4(listener, nullptr, nullptr, SOCK_NONBLOCK)), -1);
    EXPECT_EQ(errno, EAGAIN);
}

TEST(TcpListenBacklogAllocation, RepeatedLargeBacklogNeedsNoChildren) {
    Descriptors descriptors;
    // No connection demand: this should only allocate a small number of idle
    // slots, not 8 * 4097 complete TCP receive/transmit buffer pairs.
    for (int index = 0; index < 8; ++index) {
        int listener = descriptors.Add(socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0));
        ASSERT_GE(listener, 0);
        sockaddr_in address{};
        address.sin_family = AF_INET;
        address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        ASSERT_EQ(bind(listener, reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0);
        for (int repeat = 0; repeat < 8; ++repeat) {
            ASSERT_EQ(listen(listener, 4096), 0);
        }
    }
}

INSTANTIATE_TEST_SUITE_P(IPv4AndIPv6, TcpListenBacklog,
                        testing::Combine(testing::Values(AF_INET, AF_INET6),
                                         testing::Values(32, 256)));
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
