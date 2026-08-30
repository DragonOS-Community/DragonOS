#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

#include <array>
#include <cstring>

namespace {

void CreateTcpPair(int sockets[2]) {
    int listener = socket(AF_INET, SOCK_STREAM, 0);
    ASSERT_GE(listener, 0) << strerror(errno);

    sockaddr_in address = {};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    address.sin_port = 0;
    ASSERT_EQ(0, bind(listener, reinterpret_cast<sockaddr*>(&address), sizeof(address)))
        << strerror(errno);
    ASSERT_EQ(0, listen(listener, 1)) << strerror(errno);

    socklen_t address_len = sizeof(address);
    ASSERT_EQ(0, getsockname(listener, reinterpret_cast<sockaddr*>(&address), &address_len))
        << strerror(errno);

    sockets[0] = socket(AF_INET, SOCK_STREAM, 0);
    ASSERT_GE(sockets[0], 0) << strerror(errno);
    ASSERT_EQ(0, connect(sockets[0], reinterpret_cast<sockaddr*>(&address), sizeof(address)))
        << strerror(errno);
    sockets[1] = accept(listener, nullptr, nullptr);
    ASSERT_GE(sockets[1], 0) << strerror(errno);
    close(listener);
}

TEST(SocketReadvSemantics, FaultingLaterIovecPreservesUnreadData) {
    int sockets[2] = {-1, -1};
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, sockets)) << strerror(errno);

    constexpr std::array<char, 8> payload = {'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h'};
    ASSERT_EQ(static_cast<ssize_t>(payload.size()),
              write(sockets[0], payload.data(), payload.size()));

    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    void* inaccessible =
        mmap(nullptr, static_cast<size_t>(page_size), PROT_NONE,
             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, inaccessible) << strerror(errno);

    std::array<char, 4> first = {};
    iovec iovs[2] = {
        {first.data(), first.size()},
        {inaccessible, 4},
    };
    errno = 0;
    EXPECT_EQ(-1, readv(sockets[1], iovs, 2));
    EXPECT_EQ(EFAULT, errno);

    ASSERT_NE(-1, fcntl(sockets[1], F_SETFL, O_NONBLOCK)) << strerror(errno);
    std::array<char, 8> remaining = {};
    ASSERT_EQ(static_cast<ssize_t>(payload.size()),
              read(sockets[1], remaining.data(), remaining.size()));
    EXPECT_EQ(0, std::memcmp(remaining.data(), payload.data(), payload.size()));

    EXPECT_EQ(0, munmap(inaccessible, static_cast<size_t>(page_size)));
    close(sockets[0]);
    close(sockets[1]);
}

TEST(SocketReadvSemantics, ShortReadDoesNotTouchFaultingLaterIovec) {
    int sockets[2] = {-1, -1};
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, sockets)) << strerror(errno);

    constexpr std::array<char, 4> payload = {'a', 'b', 'c', 'd'};
    ASSERT_EQ(static_cast<ssize_t>(payload.size()),
              write(sockets[0], payload.data(), payload.size()));

    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    void* inaccessible =
        mmap(nullptr, static_cast<size_t>(page_size), PROT_NONE,
             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, inaccessible) << strerror(errno);

    std::array<char, 4> first = {};
    iovec iovs[2] = {{first.data(), first.size()}, {inaccessible, 4}};
    ASSERT_EQ(static_cast<ssize_t>(payload.size()), readv(sockets[1], iovs, 2));
    EXPECT_EQ(0, std::memcmp(first.data(), payload.data(), payload.size()));

    ASSERT_NE(-1, fcntl(sockets[1], F_SETFL, O_NONBLOCK)) << strerror(errno);
    char byte = 0;
    errno = 0;
    EXPECT_EQ(-1, read(sockets[1], &byte, 1));
    EXPECT_EQ(EAGAIN, errno);

    EXPECT_EQ(0, munmap(inaccessible, static_cast<size_t>(page_size)));
    close(sockets[0]);
    close(sockets[1]);
}

TEST(SocketReadvSemantics, EmptySocketResultPrecedesDestinationFault) {
    int sockets[2] = {-1, -1};
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, sockets))
        << strerror(errno);

    iovec invalid = {nullptr, 4};
    errno = 0;
    EXPECT_EQ(-1, readv(sockets[1], &invalid, 1));
    EXPECT_EQ(EAGAIN, errno);

    close(sockets[0]);
    EXPECT_EQ(0, readv(sockets[1], &invalid, 1));
    close(sockets[1]);
}

TEST(SocketReadvSemantics, TcpFaultDoesNotConsumeFailedChunk) {
    int sockets[2] = {-1, -1};
    CreateTcpPair(sockets);
    ASSERT_GE(sockets[0], 0);
    ASSERT_GE(sockets[1], 0);

    constexpr std::array<char, 8> payload = {'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h'};
    ASSERT_EQ(static_cast<ssize_t>(payload.size()),
              write(sockets[0], payload.data(), payload.size()));

    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    void* inaccessible =
        mmap(nullptr, static_cast<size_t>(page_size), PROT_NONE,
             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, inaccessible) << strerror(errno);

    std::array<char, 4> first = {};
    iovec iovs[2] = {{first.data(), first.size()}, {inaccessible, 4}};
    errno = 0;
    EXPECT_EQ(-1, readv(sockets[1], iovs, 2));
    EXPECT_EQ(EFAULT, errno);

    std::array<char, 8> remaining = {};
    ASSERT_EQ(static_cast<ssize_t>(remaining.size()),
              read(sockets[1], remaining.data(), remaining.size()));
    EXPECT_EQ(0, std::memcmp(remaining.data(), payload.data(), payload.size()));

    EXPECT_EQ(0, munmap(inaccessible, static_cast<size_t>(page_size)));
    close(sockets[0]);
    close(sockets[1]);
}

TEST(SocketReadvSemantics, UnixSeqpacketFaultConsumesRecord) {
    int sockets[2] = {-1, -1};
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sockets)) << strerror(errno);

    constexpr std::array<char, 8> payload = {'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h'};
    ASSERT_EQ(static_cast<ssize_t>(payload.size()),
              write(sockets[0], payload.data(), payload.size()));

    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    void* inaccessible =
        mmap(nullptr, static_cast<size_t>(page_size), PROT_NONE,
             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, inaccessible) << strerror(errno);

    std::array<char, 4> first = {};
    iovec iovs[2] = {{first.data(), first.size()}, {inaccessible, 4}};
    errno = 0;
    EXPECT_EQ(-1, readv(sockets[1], iovs, 2));
    EXPECT_EQ(EFAULT, errno);

    ASSERT_NE(-1, fcntl(sockets[1], F_SETFL, O_NONBLOCK)) << strerror(errno);
    char byte = 0;
    errno = 0;
    EXPECT_EQ(-1, read(sockets[1], &byte, 1));
    EXPECT_EQ(EAGAIN, errno);

    EXPECT_EQ(0, munmap(inaccessible, static_cast<size_t>(page_size)));
    close(sockets[0]);
    close(sockets[1]);
}

} // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
