#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/uio.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <chrono>
#include <cstring>
#include <thread>
#include <vector>

namespace {

bool IsDragonOS() {
    utsname name = {};
    return uname(&name) == 0 && std::strstr(name.release, "dragonos") != nullptr;
}

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

void CreateUdpPair(int sockets[2]) {
    sockets[1] = socket(AF_INET, SOCK_DGRAM, 0);
    ASSERT_GE(sockets[1], 0) << strerror(errno);

    sockaddr_in address = {};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    address.sin_port = 0;
    ASSERT_EQ(0, bind(sockets[1], reinterpret_cast<sockaddr*>(&address), sizeof(address)))
        << strerror(errno);

    socklen_t address_len = sizeof(address);
    ASSERT_EQ(0, getsockname(sockets[1], reinterpret_cast<sockaddr*>(&address), &address_len))
        << strerror(errno);

    sockets[0] = socket(AF_INET, SOCK_DGRAM, 0);
    ASSERT_GE(sockets[0], 0) << strerror(errno);
    ASSERT_EQ(0, connect(sockets[0], reinterpret_cast<sockaddr*>(&address), sizeof(address)))
        << strerror(errno);
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

TEST(SocketReadvSemantics, Preadv2CurrentOffsetFaultPreservesStream) {
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

    iovec iov = {inaccessible, payload.size()};
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_preadv2, sockets[1], &iov, 1, -1L, -1L, 0L));
    EXPECT_EQ(EFAULT, errno);

    ASSERT_NE(-1, fcntl(sockets[1], F_SETFL, O_NONBLOCK)) << strerror(errno);
    std::array<char, 8> remaining = {};
    ASSERT_EQ(static_cast<ssize_t>(remaining.size()),
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

TEST(SocketReadvSemantics, TcpWrappedRxFaultReturnsCommittedPrefix) {
    if (!IsDragonOS()) {
        GTEST_SKIP() << "smoltcp receive-ring regression test";
    }

    int sockets[2] = {-1, -1};
    CreateTcpPair(sockets);
    ASSERT_GE(sockets[0], 0);
    ASSERT_GE(sockets[1], 0);

    // DragonOS doubles SO_RCVBUF and clamps it to the Linux minimum of 2304 bytes.
    // Draining 2300 bytes leaves the smoltcp ring cursor four bytes before its end,
    // so the next eight-byte payload occupies two contiguous receive regions.
    int requested_recv_buffer = 1152;
    ASSERT_EQ(0, setsockopt(sockets[1], SOL_SOCKET, SO_RCVBUF,
                           &requested_recv_buffer, sizeof(requested_recv_buffer)))
        << strerror(errno);
    std::array<char, 2300> cursor_advance = {};
    ASSERT_EQ(static_cast<ssize_t>(cursor_advance.size()),
              write(sockets[0], cursor_advance.data(), cursor_advance.size()));
    std::array<char, 2300> drained = {};
    ASSERT_EQ(static_cast<ssize_t>(drained.size()),
              read(sockets[1], drained.data(), drained.size()));

    ASSERT_EQ(8, write(sockets[0], "abcdefgh", 8));

    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    void* inaccessible =
        mmap(nullptr, static_cast<size_t>(page_size), PROT_NONE,
             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, inaccessible) << strerror(errno);

    std::array<char, 4> first = {};
    iovec iovs[2] = {{first.data(), first.size()}, {inaccessible, 4}};
    EXPECT_EQ(4, readv(sockets[1], iovs, 2));
    EXPECT_EQ(0, std::memcmp(first.data(), "abcd", 4));

    std::array<char, 4> remaining = {};
    ASSERT_EQ(4, read(sockets[1], remaining.data(), remaining.size()));
    EXPECT_EQ(0, std::memcmp(remaining.data(), "efgh", 4));

    EXPECT_EQ(0, munmap(inaccessible, static_cast<size_t>(page_size)));
    close(sockets[0]);
    close(sockets[1]);
}

TEST(SocketReadvSemantics, ConcurrentTcpReadvDoesNotDuplicateStreamData) {
    int sockets[2] = {-1, -1};
    CreateTcpPair(sockets);
    ASSERT_GE(sockets[0], 0);
    ASSERT_GE(sockets[1], 0);

    std::array<unsigned char, 251> payload = {};
    for (size_t i = 0; i < payload.size(); ++i) {
        payload[i] = static_cast<unsigned char>(i);
    }
    ASSERT_EQ(static_cast<ssize_t>(payload.size()),
              write(sockets[0], payload.data(), payload.size()));
    ASSERT_EQ(0, shutdown(sockets[0], SHUT_WR)) << strerror(errno);

    auto reader = [&] {
        std::vector<unsigned char> received;
        for (;;) {
            unsigned char byte = 0;
            iovec iov = {&byte, sizeof(byte)};
            const ssize_t size = readv(sockets[1], &iov, 1);
            if (size == 0) {
                break;
            }
            EXPECT_EQ(1, size) << strerror(errno);
            if (size != 1) {
                break;
            }
            received.push_back(byte);
            std::this_thread::yield();
        }
        return received;
    };

    std::vector<unsigned char> first;
    std::vector<unsigned char> second;
    std::thread first_reader([&] { first = reader(); });
    std::thread second_reader([&] { second = reader(); });
    first_reader.join();
    second_reader.join();

    first.insert(first.end(), second.begin(), second.end());
    ASSERT_EQ(payload.size(), first.size());
    std::sort(first.begin(), first.end());
    EXPECT_TRUE(std::equal(first.begin(), first.end(), payload.begin()));

    close(sockets[0]);
    close(sockets[1]);
}

void ExpectRecvmsgFaultPreservesStream(int sockets[2]) {
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
    msghdr msg = {};
    msg.msg_iov = iovs;
    msg.msg_iovlen = 2;
    errno = 0;
    EXPECT_EQ(-1, recvmsg(sockets[1], &msg, 0));
    EXPECT_EQ(EFAULT, errno);

    ASSERT_NE(-1, fcntl(sockets[1], F_SETFL, O_NONBLOCK)) << strerror(errno);
    std::array<char, 8> remaining = {};
    ASSERT_EQ(static_cast<ssize_t>(remaining.size()),
              read(sockets[1], remaining.data(), remaining.size()));
    EXPECT_EQ(0, std::memcmp(remaining.data(), payload.data(), payload.size()));

    EXPECT_EQ(0, munmap(inaccessible, static_cast<size_t>(page_size)));
    close(sockets[0]);
    close(sockets[1]);
}

TEST(SocketReadvSemantics, TcpRecvmsgFaultDoesNotConsumeFailedChunk) {
    int sockets[2] = {-1, -1};
    CreateTcpPair(sockets);
    ASSERT_GE(sockets[0], 0);
    ASSERT_GE(sockets[1], 0);
    ExpectRecvmsgFaultPreservesStream(sockets);
}

TEST(SocketReadvSemantics, UnixStreamRecvmsgFaultDoesNotConsumeFailedChunk) {
    int sockets[2] = {-1, -1};
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, sockets)) << strerror(errno);
    ExpectRecvmsgFaultPreservesStream(sockets);
}

TEST(SocketReadvSemantics, TcpRecvmsgWaitAllFillsAllIovecs) {
    int sockets[2] = {-1, -1};
    CreateTcpPair(sockets);
    ASSERT_GE(sockets[0], 0);
    ASSERT_GE(sockets[1], 0);

    ASSERT_EQ(4, write(sockets[0], "abcd", 4));
    std::thread sender([&] {
        std::this_thread::sleep_for(std::chrono::milliseconds(20));
        EXPECT_EQ(4, write(sockets[0], "efgh", 4));
    });

    std::array<char, 4> first = {};
    std::array<char, 4> second = {};
    iovec iovs[2] = {{first.data(), first.size()}, {second.data(), second.size()}};
    msghdr msg = {};
    msg.msg_iov = iovs;
    msg.msg_iovlen = 2;
    EXPECT_EQ(8, recvmsg(sockets[1], &msg, MSG_WAITALL));
    EXPECT_EQ(0, std::memcmp(first.data(), "abcd", 4));
    EXPECT_EQ(0, std::memcmp(second.data(), "efgh", 4));

    sender.join();
    close(sockets[0]);
    close(sockets[1]);
}

TEST(SocketReadvSemantics, UnixSeqpacketFaultConsumesRecord) {
    for (bool use_recvmsg : {false, true}) {
        SCOPED_TRACE(use_recvmsg ? "recvmsg" : "readv");
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
        if (use_recvmsg) {
            msghdr msg = {};
            msg.msg_iov = iovs;
            msg.msg_iovlen = 2;
            EXPECT_EQ(-1, recvmsg(sockets[1], &msg, 0));
        } else {
            EXPECT_EQ(-1, readv(sockets[1], iovs, 2));
        }
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
}

void ExpectDatagramRecvmsgFaultSemantics(int sockets[2]) {
    constexpr std::array<char, 8> payload = {'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h'};
    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    void* inaccessible =
        mmap(nullptr, static_cast<size_t>(page_size), PROT_NONE,
             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, inaccessible) << strerror(errno);

    for (bool peek : {false, true}) {
        SCOPED_TRACE(peek ? "MSG_PEEK" : "consume");
        ASSERT_EQ(static_cast<ssize_t>(payload.size()),
                  send(sockets[0], payload.data(), payload.size(), 0));

        std::array<char, 4> first = {};
        iovec iovs[2] = {{first.data(), first.size()}, {inaccessible, 4}};
        msghdr msg = {};
        msg.msg_iov = iovs;
        msg.msg_iovlen = 2;
        errno = 0;
        EXPECT_EQ(-1, recvmsg(sockets[1], &msg, peek ? MSG_PEEK : 0));
        EXPECT_EQ(EFAULT, errno);

        std::array<char, 8> received = {};
        if (peek) {
            ASSERT_EQ(static_cast<ssize_t>(received.size()),
                      recv(sockets[1], received.data(), received.size(), 0));
            EXPECT_EQ(0, std::memcmp(received.data(), payload.data(), payload.size()));
        } else {
            errno = 0;
            EXPECT_EQ(-1, recv(sockets[1], received.data(), received.size(), MSG_DONTWAIT));
            EXPECT_EQ(EAGAIN, errno);
        }
    }

    EXPECT_EQ(0, munmap(inaccessible, static_cast<size_t>(page_size)));
    close(sockets[0]);
    close(sockets[1]);
}

TEST(SocketReadvSemantics, UdpRecvmsgFaultHonorsDatagramConsumption) {
    int sockets[2] = {-1, -1};
    CreateUdpPair(sockets);
    ASSERT_GE(sockets[0], 0);
    ASSERT_GE(sockets[1], 0);
    ExpectDatagramRecvmsgFaultSemantics(sockets);
}

TEST(SocketReadvSemantics, UnixDatagramRecvmsgFaultHonorsDatagramConsumption) {
    int sockets[2] = {-1, -1};
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_DGRAM, 0, sockets)) << strerror(errno);
    ExpectDatagramRecvmsgFaultSemantics(sockets);
}

} // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
