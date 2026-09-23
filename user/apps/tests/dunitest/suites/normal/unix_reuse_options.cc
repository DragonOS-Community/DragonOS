#include <gtest/gtest.h>

#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <cstddef>
#include <cstdlib>
#include <cstring>
#include <string>

namespace {

struct Fd {
    int value = -1;
    explicit Fd(int fd = -1) : value(fd) {}
    ~Fd() {
        if (value >= 0) close(value);
    }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
};

int ReadIntOption(int fd, int option) {
    int value = -1;
    socklen_t length = sizeof(value);
    EXPECT_EQ(getsockopt(fd, SOL_SOCKET, option, &value, &length), 0)
        << std::strerror(errno);
    EXPECT_EQ(length, sizeof(value));
    return value;
}

class UnixReuseOptions : public testing::TestWithParam<int> {};

TEST_P(UnixReuseOptions, PerSocketBooleanStateAndReuseportBoundary) {
    int pair[2] = {-1, -1};
    ASSERT_EQ(socketpair(AF_UNIX, GetParam(), 0, pair), 0) << std::strerror(errno);
    Fd first(pair[0]);
    Fd second(pair[1]);

    EXPECT_EQ(ReadIntOption(first.value, SO_REUSEADDR), 0);
    EXPECT_EQ(ReadIntOption(second.value, SO_REUSEADDR), 0);
    EXPECT_EQ(ReadIntOption(first.value, SO_REUSEPORT), 0);
    Fd alias(dup(first.value));
    ASSERT_GE(alias.value, 0);

    for (int value : {1, -2, 0}) {
        ASSERT_EQ(setsockopt(first.value, SOL_SOCKET, SO_REUSEADDR, &value, sizeof(value)), 0)
            << std::strerror(errno);
        EXPECT_EQ(ReadIntOption(first.value, SO_REUSEADDR), value != 0);
        EXPECT_EQ(ReadIntOption(alias.value, SO_REUSEADDR), value != 0);
        EXPECT_EQ(ReadIntOption(second.value, SO_REUSEADDR), 0);
    }

    int one = 1;
    errno = 0;
    EXPECT_EQ(setsockopt(first.value, SOL_SOCKET, SO_REUSEPORT, &one, sizeof(one)), -1);
    EXPECT_EQ(errno, EOPNOTSUPP);
    EXPECT_EQ(ReadIntOption(first.value, SO_REUSEPORT), 0);
    int zero = 0;
    EXPECT_EQ(setsockopt(first.value, SOL_SOCKET, SO_REUSEPORT, &zero, sizeof(zero)), 0);
    EXPECT_EQ(ReadIntOption(first.value, SO_REUSEPORT), 0);

    for (int option : {SO_REUSEADDR, SO_REUSEPORT}) {
        errno = 0;
        EXPECT_EQ(setsockopt(first.value, SOL_SOCKET, option, &one, sizeof(one) - 1), -1);
        EXPECT_EQ(errno, EINVAL);
    }
}

TEST_P(UnixReuseOptions, GetsockoptCopiesOnlyRequestedBytes) {
    Fd socket_fd(socket(AF_UNIX, GetParam(), 0));
    ASSERT_GE(socket_fd.value, 0) << std::strerror(errno);
    int one = 1;
    ASSERT_EQ(setsockopt(socket_fd.value, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one)), 0);

    for (socklen_t capacity : {0u, 1u, 2u, 3u, 4u, 8u}) {
        std::array<unsigned char, 8> bytes;
        bytes.fill(0x7a);
        socklen_t length = capacity;
        ASSERT_EQ(getsockopt(socket_fd.value, SOL_SOCKET, SO_REUSEADDR, bytes.data(), &length), 0)
            << "capacity=" << capacity << ": " << std::strerror(errno);
        const size_t copied = std::min<size_t>(capacity, sizeof(one));
        EXPECT_EQ(length, copied);
        EXPECT_EQ(std::memcmp(bytes.data(), &one, copied), 0);
        for (size_t index = copied; index < bytes.size(); ++index) {
            EXPECT_EQ(bytes[index], 0x7a) << "capacity=" << capacity;
        }
    }
}

INSTANTIATE_TEST_SUITE_P(SocketTypes, UnixReuseOptions,
                         testing::Values(SOCK_STREAM, SOCK_DGRAM, SOCK_SEQPACKET));

TEST(UnixReuseOptions, ListenerStateDoesNotChangePathOwnershipOrAcceptedSocket) {
    char directory[] = "/tmp/dkc003-XXXXXX";
    ASSERT_NE(mkdtemp(directory), nullptr) << std::strerror(errno);
    const std::string path = std::string(directory) + "/socket";

    Fd listener(socket(AF_UNIX, SOCK_STREAM, 0));
    Fd duplicate(socket(AF_UNIX, SOCK_STREAM, 0));
    Fd client(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(listener.value, 0);
    ASSERT_GE(duplicate.value, 0);
    ASSERT_GE(client.value, 0);

    int one = 1;
    ASSERT_EQ(setsockopt(listener.value, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one)), 0);
    ASSERT_EQ(setsockopt(duplicate.value, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one)), 0);

    sockaddr_un address{};
    address.sun_family = AF_UNIX;
    ASSERT_LT(path.size(), sizeof(address.sun_path));
    std::memcpy(address.sun_path, path.c_str(), path.size() + 1);
    const auto length = static_cast<socklen_t>(offsetof(sockaddr_un, sun_path) + path.size() + 1);

    ASSERT_EQ(bind(listener.value, reinterpret_cast<sockaddr*>(&address), length), 0)
        << std::strerror(errno);
    errno = 0;
    EXPECT_EQ(bind(duplicate.value, reinterpret_cast<sockaddr*>(&address), length), -1);
    EXPECT_EQ(errno, EADDRINUSE);
    ASSERT_EQ(listen(listener.value, 1), 0);
    EXPECT_EQ(ReadIntOption(listener.value, SO_REUSEADDR), 1);
    ASSERT_EQ(connect(client.value, reinterpret_cast<sockaddr*>(&address), length), 0);
    Fd accepted(accept(listener.value, nullptr, nullptr));
    ASSERT_GE(accepted.value, 0) << std::strerror(errno);
    EXPECT_EQ(ReadIntOption(accepted.value, SO_REUSEADDR), 0);

    int zero = 0;
    ASSERT_EQ(setsockopt(listener.value, SOL_SOCKET, SO_REUSEADDR, &zero, sizeof(zero)), 0);
    EXPECT_EQ(ReadIntOption(listener.value, SO_REUSEADDR), 0);
    EXPECT_EQ(ReadIntOption(accepted.value, SO_REUSEADDR), 0);

    EXPECT_EQ(unlink(path.c_str()), 0);
    EXPECT_EQ(rmdir(directory), 0);
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
