#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cerrno>
#include <chrono>
#include <functional>
#include <thread>
#include <vector>

namespace {
// A broken nonblocking syscall must fail the test, not hang the suite.
void Bounded(const std::function<void()>& body) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        body();
        _exit(testing::Test::HasFailure() ? 1 : 0);
    }
    auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(3);
    int status = 0;
    while (std::chrono::steady_clock::now() < deadline) {
        pid_t result = waitpid(child, &status, WNOHANG);
        if (result == child) {
            ASSERT_TRUE(WIFEXITED(status));
            EXPECT_EQ(0, WEXITSTATUS(status));
            return;
        }
        if (result < 0 && errno != EINTR) break;
        usleep(10000);
    }
    kill(child, SIGKILL);
    while (waitpid(child, &status, 0) < 0 && errno == EINTR) {}
    FAIL() << "socket operation did not finish within three seconds";
}

class Sockets {
 public:
    ~Sockets() { for (int fd : owned) close(fd); }
    int Keep(int fd) { if (fd >= 0) owned.push_back(fd); return fd; }
    int New(int family, int type) { return Keep(socket(family, type, 0)); }
    void Listen(int* fd, sockaddr_in* address) {
        *fd = New(AF_INET, SOCK_STREAM);
        ASSERT_GE(*fd, 0);
        *address = {};
        address->sin_family = AF_INET;
        address->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        ASSERT_EQ(0, bind(*fd, reinterpret_cast<sockaddr*>(address), sizeof(*address)));
        socklen_t size = sizeof(*address);
        ASSERT_EQ(0, getsockname(*fd, reinterpret_cast<sockaddr*>(address), &size));
        ASSERT_EQ(0, listen(*fd, 4));
    }
    void Pair(int kind, int* pair) {
        if (kind >= 2) {
            ASSERT_EQ(0, socketpair(AF_UNIX, kind == 2 ? SOCK_STREAM : SOCK_DGRAM, 0, pair));
            Keep(pair[0]);
            Keep(pair[1]);
        } else if (kind == 0) {
            int listener;
            sockaddr_in address;
            ASSERT_NO_FATAL_FAILURE(Listen(&listener, &address));
            pair[1] = New(AF_INET, SOCK_STREAM);
            ASSERT_GE(pair[1], 0);
            ASSERT_EQ(0, connect(pair[1], reinterpret_cast<sockaddr*>(&address), sizeof(address)));
            pollfd ready{listener, POLLIN, 0};
            ASSERT_EQ(1, poll(&ready, 1, 1000));
            pair[0] = Keep(accept(listener, nullptr, nullptr));
            ASSERT_GE(pair[0], 0);
        } else {
            sockaddr_in addresses[2]{};
            for (int i = 0; i < 2; ++i) {
                pair[i] = New(AF_INET, SOCK_DGRAM);
                ASSERT_GE(pair[i], 0);
                addresses[i].sin_family = AF_INET;
                addresses[i].sin_addr.s_addr = htonl(INADDR_LOOPBACK);
                ASSERT_EQ(0, bind(pair[i], reinterpret_cast<sockaddr*>(&addresses[i]), sizeof(addresses[i])));
                socklen_t size = sizeof(addresses[i]);
                ASSERT_EQ(0, getsockname(pair[i], reinterpret_cast<sockaddr*>(&addresses[i]), &size));
            }
            for (int i = 0; i < 2; ++i)
                ASSERT_EQ(0, connect(pair[i], reinterpret_cast<sockaddr*>(&addresses[1-i]), sizeof(addresses[i])));
        }
    }
 private:
    std::vector<int> owned;
};

class SocketNonblocking : public testing::TestWithParam<int> {};

// Exercise file-style socket reads as used by TLS BIOs, not just recv().
void CheckFileReadNonblocking(int kind, ssize_t (*read_bytes)(int, char*)) {
    Bounded([&] {
        Sockets sockets;
        int pair[2];
        ASSERT_NO_FATAL_FAILURE(sockets.Pair(kind, pair));
        int alias = sockets.Keep(dup(pair[0]));
        ASSERT_GE(alias, 0);
        int enabled = 1;
        ASSERT_EQ(0, ioctl(pair[0], FIONBIO, &enabled));
        int flags = fcntl(alias, F_GETFL);
        ASSERT_GE(flags, 0);
        ASSERT_NE(0, flags & O_NONBLOCK);

        char bytes[2] = {};
        ASSERT_EQ(-1, read_bytes(alias, bytes));
        ASSERT_EQ(EAGAIN, errno);
        // One byte avoids assuming that a stream read fills its buffer.
        ASSERT_EQ(1, send(pair[1], "x", 1, MSG_NOSIGNAL));
        pollfd ready{alias, POLLIN, 0};
        ASSERT_EQ(1, poll(&ready, 1, 1000));
        ASSERT_EQ(1, read_bytes(alias, bytes));
        EXPECT_EQ('x', bytes[0]);
        EXPECT_EQ(0, bytes[1]);
        ASSERT_EQ(-1, read_bytes(alias, bytes));
        ASSERT_EQ(EAGAIN, errno);

        enabled = 0;
        ASSERT_EQ(0, ioctl(alias, FIONBIO, &enabled));
        flags = fcntl(pair[0], F_GETFL);
        ASSERT_GE(flags, 0);
        ASSERT_EQ(0, flags & O_NONBLOCK);
        ssize_t sent = -1;
        std::thread writer([&] {
            usleep(50000);
            sent = send(pair[1], "y", 1, MSG_NOSIGNAL);
        });
        ssize_t received = read_bytes(pair[0], bytes);
        int error = errno;
        writer.join();
        ASSERT_EQ(1, sent);
        ASSERT_EQ(1, received) << "errno=" << error;
        EXPECT_EQ('y', bytes[0]);
        EXPECT_EQ(0, bytes[1]);
    });
}

TEST_P(SocketNonblocking, IoctlToggleAndDrainAffectRead) {
    CheckFileReadNonblocking(GetParam(), [](int fd, char* bytes) {
        return read(fd, bytes, 2);
    });
}

TEST_P(SocketNonblocking, IoctlToggleAndDrainAffectReadv) {
    CheckFileReadNonblocking(GetParam(), [](int fd, char* bytes) {
        iovec buffers[] = {{bytes, 1}, {bytes + 1, 1}};
        return readv(fd, buffers, 2);
    });
}

TEST_P(SocketNonblocking, IoctlToggleAndDupAffectReceive) {
    Bounded([&] {
        Sockets sockets;
        int pair[2];
        ASSERT_NO_FATAL_FAILURE(sockets.Pair(GetParam(), pair));
        int alias = sockets.Keep(dup(pair[0]));
        ASSERT_GE(alias, 0);
        int enabled = 1;
        ASSERT_EQ(0, ioctl(pair[0], FIONBIO, &enabled));
        int flags = fcntl(alias, F_GETFL);
        ASSERT_GE(flags, 0);
        ASSERT_NE(0, flags & O_NONBLOCK);
        char byte;
        ASSERT_EQ(-1, recv(alias, &byte, 1, 0));
        ASSERT_EQ(EAGAIN, errno);
        enabled = 0;
        ASSERT_EQ(0, ioctl(alias, FIONBIO, &enabled));
        ASSERT_EQ(0, fcntl(pair[0], F_GETFL) & O_NONBLOCK);
        ASSERT_EQ(0, fcntl(alias, F_GETFL) & O_NONBLOCK);
        // No data is present yet: a wrongly retained nonblocking state returns
        // EAGAIN instead of waiting for this delayed byte.
        ssize_t sent = -1;
        std::thread writer([&] {
            usleep(50000);
            sent = send(pair[1], "x", 1, MSG_NOSIGNAL);
        });
        ssize_t received = recv(pair[0], &byte, 1, 0);
        writer.join();
        ASSERT_EQ(1, sent);
        ASSERT_EQ(1, received);
        EXPECT_EQ('x', byte);
    });
}

std::string ProtocolName(const testing::TestParamInfo<int>& info) {
    const char* names[] = {"Tcp", "Udp", "UnixStream", "UnixDatagram"};
    return names[info.param];
}
INSTANTIATE_TEST_SUITE_P(Protocols, SocketNonblocking, testing::Values(0, 1, 2, 3), ProtocolName);

TEST_P(SocketNonblocking, CreationFlagsMatchBehavior) {
    Bounded([&] {
        Sockets sockets;
        int descriptors[2];
        int count = 1;
        if (GetParam() >= 2) {
            int type = GetParam() == 2 ? SOCK_STREAM : SOCK_DGRAM;
            ASSERT_EQ(0, socketpair(AF_UNIX, type | SOCK_NONBLOCK, 0, descriptors));
            sockets.Keep(descriptors[0]);
            sockets.Keep(descriptors[1]);
            count = 2;
        } else {
            int type = GetParam() == 0 ? SOCK_STREAM : SOCK_DGRAM;
            descriptors[0] = sockets.New(AF_INET, type | SOCK_NONBLOCK);
            ASSERT_GE(descriptors[0], 0);
            sockaddr_in address{};
            address.sin_family = AF_INET;
            address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
            ASSERT_EQ(0, bind(descriptors[0], reinterpret_cast<sockaddr*>(&address), sizeof(address)));
            if (GetParam() == 0) {
                ASSERT_EQ(0, listen(descriptors[0], 4));
            }
        }
        for (int i = 0; i < count; ++i) {
            int flags = fcntl(descriptors[i], F_GETFL);
            ASSERT_GE(flags, 0);
            ASSERT_NE(0, flags & O_NONBLOCK);
            if (GetParam() == 0) {
                int accepted = accept(descriptors[i], nullptr, nullptr);
                int error = errno;
                if (accepted >= 0) close(accepted);
                ASSERT_EQ(-1, accepted);
                ASSERT_EQ(EAGAIN, error);
            } else {
                char byte = 0;
                ASSERT_EQ(-1, recv(descriptors[i], &byte, 1, 0));
                ASSERT_EQ(EAGAIN, errno);
            }
        }
    });
}

class ListenerNonblocking : public testing::TestWithParam<bool> {};
TEST_P(ListenerNonblocking, IoctlMakesEmptyAcceptReturnAgain) {
    Bounded([&] {
        Sockets sockets;
        int listener;
        sockaddr_in address;
        ASSERT_NO_FATAL_FAILURE(sockets.Listen(&listener, &address));
        int enabled = 1;
        ASSERT_EQ(0, ioctl(listener, FIONBIO, &enabled));
        int accepted = GetParam() ? accept4(listener, nullptr, nullptr, SOCK_NONBLOCK)
                                  : accept(listener, nullptr, nullptr);
        int error = errno;
        if (accepted >= 0) close(accepted);
        ASSERT_EQ(-1, accepted);
        ASSERT_EQ(EAGAIN, error);
    });
}
TEST_P(ListenerNonblocking, ConsumedReadinessDoesNotBlockControlEvents) {
    Bounded([&] {
        Sockets sockets;
        int listener;
        sockaddr_in address;
        ASSERT_NO_FATAL_FAILURE(sockets.Listen(&listener, &address));
        int alias = sockets.Keep(dup(listener));
        ASSERT_GE(alias, 0);
        int enabled = 1;
        ASSERT_EQ(0, ioctl(listener, FIONBIO, &enabled));

        int control[2];
        ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, control));
        sockets.Keep(control[0]);
        sockets.Keep(control[1]);
        int epoll = sockets.Keep(epoll_create1(EPOLL_CLOEXEC));
        ASSERT_GE(epoll, 0);
        for (int fd : {listener, control[0]}) {
            epoll_event event{};
            event.events = EPOLLIN;
            event.data.fd = fd;
            ASSERT_EQ(0, epoll_ctl(epoll, EPOLL_CTL_ADD, fd, &event));
        }

        int client = sockets.New(AF_INET, SOCK_STREAM);
        ASSERT_GE(client, 0);
        ASSERT_EQ(0, connect(client, reinterpret_cast<sockaddr*>(&address), sizeof(address)));
        epoll_event event{};
        ASSERT_EQ(1, epoll_wait(epoll, &event, 1, 1000));
        ASSERT_EQ(listener, event.data.fd);
        ASSERT_NE(0u, event.events & EPOLLIN);

        // Another user of the shared listener can consume a reported event.
        // Keep the connection alive so EOF cannot supply an unrelated event.
        ASSERT_GE(sockets.Keep(accept(alias, nullptr, nullptr)), 0);
        ASSERT_EQ(1, send(control[1], "q", 1, MSG_NOSIGNAL));
        int accepted = GetParam() ? accept4(listener, nullptr, nullptr, SOCK_NONBLOCK)
                                  : accept(listener, nullptr, nullptr);
        int error = errno;
        sockets.Keep(accepted);
        ASSERT_EQ(-1, accepted);
        ASSERT_EQ(EAGAIN, error);

        // A stale listener event must not prevent an event-driven worker from
        // returning to epoll and handling its control channel.
        event = {};
        ASSERT_EQ(1, epoll_wait(epoll, &event, 1, 1000));
        ASSERT_EQ(control[0], event.data.fd);
        ASSERT_NE(0u, event.events & EPOLLIN);
        char command = 0;
        ASSERT_EQ(1, recv(control[0], &command, 1, 0));
        EXPECT_EQ('q', command);
    });
}

INSTANTIATE_TEST_SUITE_P(Calls, ListenerNonblocking, testing::Bool());

TEST(SocketNonblockingFlags, AcceptedSocketFlagsAreExplicit) {
    Bounded([] {
        Sockets sockets;
        int listener;
        sockaddr_in address;
        ASSERT_NO_FATAL_FAILURE(sockets.Listen(&listener, &address));
        int enabled = 1;
        ASSERT_EQ(0, ioctl(listener, FIONBIO, &enabled));
        for (int flags : {0, SOCK_NONBLOCK | SOCK_CLOEXEC}) {
            int client = sockets.New(AF_INET, SOCK_STREAM);
            ASSERT_GE(client, 0);
            ASSERT_EQ(0, connect(client, reinterpret_cast<sockaddr*>(&address), sizeof(address)));
            pollfd ready{listener, POLLIN, 0};
            ASSERT_EQ(1, poll(&ready, 1, 1000));
            int accepted = sockets.Keep(flags ? accept4(listener, nullptr, nullptr, flags)
                                              : accept(listener, nullptr, nullptr));
            ASSERT_GE(accepted, 0);
            int status = fcntl(accepted, F_GETFL);
            int descriptor = fcntl(accepted, F_GETFD);
            ASSERT_GE(status, 0);
            ASSERT_GE(descriptor, 0);
            EXPECT_EQ(flags != 0, (status & O_NONBLOCK) != 0);
            EXPECT_EQ(flags != 0, (descriptor & FD_CLOEXEC) != 0);
            char byte = 0;
            if (flags) {
                ASSERT_EQ(-1, recv(accepted, &byte, 1, 0));
                ASSERT_EQ(EAGAIN, errno);
            } else {
                ssize_t sent = -1;
                std::thread writer([&] {
                    usleep(50000);
                    sent = send(client, "x", 1, MSG_NOSIGNAL);
                });
                ssize_t received = recv(accepted, &byte, 1, 0);
                writer.join();
                ASSERT_EQ(1, sent);
                ASSERT_EQ(1, received);
                EXPECT_EQ('x', byte);
            }
        }
    });
}
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
