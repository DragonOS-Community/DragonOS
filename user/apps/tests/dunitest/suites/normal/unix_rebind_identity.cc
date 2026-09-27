#include <gtest/gtest.h>

#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

struct Fd {
    int fd;
    explicit Fd(int value) : fd(value) {}
    ~Fd() { if (fd >= 0) close(fd); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
};

sockaddr_un PathAddress(const char* path) {
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    snprintf(addr.sun_path, sizeof(addr.sun_path), "%s", path);
    return addr;
}

sockaddr_un AbstractAddress(const char* name) {
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    snprintf(addr.sun_path + 1, sizeof(addr.sun_path) - 1, "%s", name);
    return addr;
}

socklen_t AbstractLength(const sockaddr_un& addr) {
    return offsetof(sockaddr_un, sun_path) + 1 + strlen(addr.sun_path + 1);
}

TEST(UnixRebindIdentity, StreamSamePathDifferentInode) {
    char path[100];
    snprintf(path, sizeof(path), "/tmp/dkc034-stream-%d", getpid());
    unlink(path);
    const auto addr = PathAddress(path);
    Fd old_listener(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(old_listener.fd, 0);
    ASSERT_EQ(0, bind(old_listener.fd, reinterpret_cast<const sockaddr*>(&addr), sizeof(addr))) << strerror(errno);
    ASSERT_EQ(0, listen(old_listener.fd, 2)) << strerror(errno);
    ASSERT_EQ(0, unlink(path));
    Fd new_listener(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(new_listener.fd, 0);
    ASSERT_EQ(0, bind(new_listener.fd, reinterpret_cast<const sockaddr*>(&addr), sizeof(addr))) << strerror(errno);
    EXPECT_EQ(0, listen(new_listener.fd, 2)) << strerror(errno);
    Fd client(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(client.fd, 0);
    EXPECT_EQ(0, connect(client.fd, reinterpret_cast<const sockaddr*>(&addr), sizeof(addr))) << strerror(errno);
    unlink(path);
}

TEST(UnixRebindIdentity, DatagramSamePathDifferentInode) {
    char path[100];
    snprintf(path, sizeof(path), "/tmp/dkc034-dgram-%d", getpid());
    unlink(path);
    const auto addr = PathAddress(path);
    Fd old_socket(socket(AF_UNIX, SOCK_DGRAM, 0));
    ASSERT_GE(old_socket.fd, 0);
    ASSERT_EQ(0, bind(old_socket.fd, reinterpret_cast<const sockaddr*>(&addr), sizeof(addr))) << strerror(errno);
    ASSERT_EQ(0, unlink(path));
    Fd new_socket(socket(AF_UNIX, SOCK_DGRAM, 0));
    ASSERT_GE(new_socket.fd, 0);
    ASSERT_EQ(0, bind(new_socket.fd, reinterpret_cast<const sockaddr*>(&addr), sizeof(addr))) << strerror(errno);
    Fd sender(socket(AF_UNIX, SOCK_DGRAM, 0));
    ASSERT_GE(sender.fd, 0);
    EXPECT_EQ(1, sendto(sender.fd, "x", 1, 0, reinterpret_cast<const sockaddr*>(&addr), sizeof(addr))) << strerror(errno);
    char byte = 0;
    EXPECT_EQ(1, recv(new_socket.fd, &byte, 1, MSG_DONTWAIT)) << strerror(errno);
    EXPECT_EQ('x', byte);
    close(old_socket.fd);
    old_socket.fd = -1;
    EXPECT_EQ(1, sendto(sender.fd, "y", 1, 0, reinterpret_cast<const sockaddr*>(&addr), sizeof(addr))) << strerror(errno);
    EXPECT_EQ(1, recv(new_socket.fd, &byte, 1, MSG_DONTWAIT)) << strerror(errno);
    EXPECT_EQ('y', byte);
    unlink(path);
}

TEST(UnixRebindIdentity, ConnectedDatagramStaysWithOldInstance) {
    char path[100];
    snprintf(path, sizeof(path), "/tmp/dkc034-peer-%d", getpid());
    unlink(path);
    const auto addr = PathAddress(path);
    Fd old_socket(socket(AF_UNIX, SOCK_DGRAM, 0));
    ASSERT_GE(old_socket.fd, 0);
    ASSERT_EQ(0, bind(old_socket.fd, reinterpret_cast<const sockaddr*>(&addr), sizeof(addr)));
    Fd sender(socket(AF_UNIX, SOCK_DGRAM, 0));
    ASSERT_GE(sender.fd, 0);
    ASSERT_EQ(0, connect(sender.fd, reinterpret_cast<const sockaddr*>(&addr), sizeof(addr)));
    ASSERT_EQ(0, unlink(path));
    Fd replacement(socket(AF_UNIX, SOCK_DGRAM, 0));
    ASSERT_GE(replacement.fd, 0);
    ASSERT_EQ(0, bind(replacement.fd, reinterpret_cast<const sockaddr*>(&addr), sizeof(addr)));
    EXPECT_EQ(1, send(sender.fd, "o", 1, 0)) << strerror(errno);
    char byte = 0;
    EXPECT_EQ(1, recv(old_socket.fd, &byte, 1, MSG_DONTWAIT)) << strerror(errno);
    EXPECT_EQ('o', byte);
    EXPECT_EQ(-1, recv(replacement.fd, &byte, 1, MSG_DONTWAIT));
    EXPECT_EQ(EAGAIN, errno);
    unlink(path);
}

TEST(UnixRebindIdentity, StreamRenamedPathResolvesSameInode) {
    char old_path[100], new_path[100];
    snprintf(old_path, sizeof(old_path), "/tmp/dkc034-rename-old-%d", getpid());
    snprintf(new_path, sizeof(new_path), "/tmp/dkc034-rename-new-%d", getpid());
    unlink(old_path);
    unlink(new_path);
    const auto old_addr = PathAddress(old_path);
    const auto new_addr = PathAddress(new_path);
    Fd listener(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(listener.fd, 0);
    ASSERT_EQ(0, bind(listener.fd, reinterpret_cast<const sockaddr*>(&old_addr), sizeof(old_addr))) << strerror(errno);
    ASSERT_EQ(0, listen(listener.fd, 2));
    ASSERT_EQ(0, rename(old_path, new_path)) << strerror(errno);
    Fd client(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(client.fd, 0);
    EXPECT_EQ(0, connect(client.fd, reinterpret_cast<const sockaddr*>(&new_addr), sizeof(new_addr))) << strerror(errno);
    unlink(new_path);
}

TEST(UnixRebindIdentity, AbstractNameReleasedWithListener) {
    char name[90];
    snprintf(name, sizeof(name), "dkc034-abstract-%d", getpid());
    const auto addr = AbstractAddress(name);
    const auto len = AbstractLength(addr);
    int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GE(listener, 0);
    ASSERT_EQ(0, bind(listener, reinterpret_cast<const sockaddr*>(&addr), len));
    ASSERT_EQ(0, listen(listener, 2));
    Fd client(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(client.fd, 0);
    ASSERT_EQ(0, connect(client.fd, reinterpret_cast<const sockaddr*>(&addr), len));
    Fd accepted(accept(listener, nullptr, nullptr));
    ASSERT_GE(accepted.fd, 0);
    close(listener);
    Fd replacement(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(replacement.fd, 0);
    EXPECT_EQ(0, bind(replacement.fd, reinterpret_cast<const sockaddr*>(&addr), len)) << strerror(errno);
}

TEST(UnixRebindIdentity, AbstractDatagramRebindDoesNotRetargetOldPeer) {
    char name[90];
    snprintf(name, sizeof(name), "dkc034-dgram-abstract-%d", getpid());
    const auto addr = AbstractAddress(name);
    const auto len = AbstractLength(addr);
    int old_socket = socket(AF_UNIX, SOCK_DGRAM, 0);
    ASSERT_GE(old_socket, 0);
    ASSERT_EQ(0, bind(old_socket, reinterpret_cast<const sockaddr*>(&addr), len));
    Fd sender(socket(AF_UNIX, SOCK_DGRAM, 0));
    ASSERT_GE(sender.fd, 0);
    ASSERT_EQ(0, connect(sender.fd, reinterpret_cast<const sockaddr*>(&addr), len));
    close(old_socket);
    Fd replacement(socket(AF_UNIX, SOCK_DGRAM, 0));
    ASSERT_GE(replacement.fd, 0);
    ASSERT_EQ(0, bind(replacement.fd, reinterpret_cast<const sockaddr*>(&addr), len)) << strerror(errno);
    EXPECT_EQ(-1, send(sender.fd, "x", 1, 0));
    EXPECT_EQ(ECONNREFUSED, errno);
    EXPECT_EQ(-1, send(sender.fd, "x", 1, 0));
    EXPECT_EQ(ENOTCONN, errno);
    char byte = 0;
    EXPECT_EQ(-1, recv(replacement.fd, &byte, 1, MSG_DONTWAIT));
    EXPECT_EQ(EAGAIN, errno);
}

TEST(UnixRebindIdentity, AbstractNameIsScopedBySocketType) {
    char name[90];
    snprintf(name, sizeof(name), "dkc034-types-%d", getpid());
    const auto addr = AbstractAddress(name);
    const auto len = AbstractLength(addr);
    Fd stream(socket(AF_UNIX, SOCK_STREAM, 0));
    Fd datagram(socket(AF_UNIX, SOCK_DGRAM, 0));
    Fd seqpacket(socket(AF_UNIX, SOCK_SEQPACKET, 0));
    ASSERT_GE(stream.fd, 0);
    ASSERT_GE(datagram.fd, 0);
    ASSERT_GE(seqpacket.fd, 0);
    ASSERT_EQ(0, bind(stream.fd, reinterpret_cast<const sockaddr*>(&addr), len)) << strerror(errno);
    ASSERT_EQ(0, bind(datagram.fd, reinterpret_cast<const sockaddr*>(&addr), len)) << strerror(errno);
    ASSERT_EQ(0, bind(seqpacket.fd, reinterpret_cast<const sockaddr*>(&addr), len)) << strerror(errno);
    ASSERT_EQ(0, listen(stream.fd, 2));
    ASSERT_EQ(0, listen(seqpacket.fd, 2));
    Fd stream_client(socket(AF_UNIX, SOCK_STREAM, 0));
    Fd seqpacket_client(socket(AF_UNIX, SOCK_SEQPACKET, 0));
    ASSERT_GE(stream_client.fd, 0);
    ASSERT_GE(seqpacket_client.fd, 0);
    EXPECT_EQ(0, connect(stream_client.fd, reinterpret_cast<const sockaddr*>(&addr), len)) << strerror(errno);
    EXPECT_EQ(0, connect(seqpacket_client.fd, reinterpret_cast<const sockaddr*>(&addr), len)) << strerror(errno);
}

TEST(UnixRebindIdentity, BlockedConnectRechecksAfterReplacement) {
    char path[100];
    snprintf(path, sizeof(path), "/tmp/dkc034-retry-%d", getpid());
    unlink(path);
    const auto addr = PathAddress(path);
    int old_listener = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GE(old_listener, 0);
    ASSERT_EQ(0, bind(old_listener, reinterpret_cast<const sockaddr*>(&addr), sizeof(addr)));
    ASSERT_EQ(0, listen(old_listener, 0));
    Fd first(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(first.fd, 0);
    ASSERT_EQ(0, connect(first.fd, reinterpret_cast<const sockaddr*>(&addr), sizeof(addr)));
    int results[2];
    ASSERT_EQ(0, pipe(results));
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(results[0]);
        close(old_listener);
        const int client = socket(AF_UNIX, SOCK_STREAM, 0);
        const timeval timeout{2, 0};
        if (client < 0 || setsockopt(client, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout))) _exit(2);
        const int status = connect(client, reinterpret_cast<const sockaddr*>(&addr), sizeof(addr));
        const int result = status == 0 ? 0 : errno;
        if (write(results[1], &result, sizeof(result)) != sizeof(result)) _exit(3);
        close(client);
        _exit(0);
    }
    close(results[1]);
    pollfd waiter{results[0], POLLIN, 0};
    EXPECT_EQ(0, poll(&waiter, 1, 50));
    ASSERT_EQ(0, unlink(path));
    Fd replacement(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(replacement.fd, 0);
    ASSERT_EQ(0, bind(replacement.fd, reinterpret_cast<const sockaddr*>(&addr), sizeof(addr)));
    ASSERT_EQ(0, listen(replacement.fd, 2));
    close(old_listener);
    EXPECT_EQ(1, poll(&waiter, 1, 3000));
    int result = -1;
    if (read(results[0], &result, sizeof(result)) == sizeof(result)) EXPECT_EQ(0, result);
    else ADD_FAILURE() << "child did not report connect result";
    close(results[0]);
    int status = 0;
    waitpid(child, &status, 0);
    EXPECT_TRUE(WIFEXITED(status));
    unlink(path);
}

TEST(UnixRebindIdentity, BlockedAbstractConnectDoesNotKeepName) {
    char name[90];
    snprintf(name, sizeof(name), "dkc034-blocked-abstract-%d", getpid());
    const auto addr = AbstractAddress(name);
    const auto len = AbstractLength(addr);
    int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GE(listener, 0);
    ASSERT_EQ(0, bind(listener, reinterpret_cast<const sockaddr*>(&addr), len));
    ASSERT_EQ(0, listen(listener, 0));
    Fd first(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(first.fd, 0);
    ASSERT_EQ(0, connect(first.fd, reinterpret_cast<const sockaddr*>(&addr), len));

    int result_pipe[2];
    ASSERT_EQ(0, pipe(result_pipe));
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(result_pipe[0]);
        close(listener);
        const int client = socket(AF_UNIX, SOCK_STREAM, 0);
        const timeval timeout{2, 0};
        if (client < 0 || setsockopt(client, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout))) _exit(2);
        const int ready = -2;
        if (write(result_pipe[1], &ready, sizeof(ready)) != sizeof(ready)) _exit(3);
        const int status = connect(client, reinterpret_cast<const sockaddr*>(&addr), len);
        const int result = status == 0 ? 0 : errno;
        if (write(result_pipe[1], &result, sizeof(result)) != sizeof(result)) _exit(4);
        close(client);
        _exit(0);
    }

    close(result_pipe[1]);
    int ready = 0;
    ASSERT_EQ(static_cast<ssize_t>(sizeof(ready)), read(result_pipe[0], &ready, sizeof(ready)));
    ASSERT_EQ(-2, ready);
    pollfd waiter{result_pipe[0], POLLIN, 0};
    EXPECT_EQ(0, poll(&waiter, 1, 50));
    close(listener);
    Fd replacement(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(replacement.fd, 0);
    EXPECT_EQ(0, bind(replacement.fd, reinterpret_cast<const sockaddr*>(&addr), len)) << strerror(errno);
    EXPECT_EQ(0, listen(replacement.fd, 2));
    EXPECT_EQ(1, poll(&waiter, 1, 3000));
    int result = -1;
    EXPECT_EQ(static_cast<ssize_t>(sizeof(result)), read(result_pipe[0], &result, sizeof(result)));
    close(result_pipe[0]);
    int status = 0;
    waitpid(child, &status, 0);
    EXPECT_TRUE(WIFEXITED(status));
}

} // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
