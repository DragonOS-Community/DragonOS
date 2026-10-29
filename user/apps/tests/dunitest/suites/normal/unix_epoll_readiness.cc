#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <array>

namespace {

int WaitEvents(int epfd, uint32_t* events, int timeout_ms = 500) {
    epoll_event event = {};
    const int count = epoll_wait(epfd, &event, 1, timeout_ms);
    if (count == 1) {
        *events = event.events;
    }
    return count;
}

bool SetNonblocking(int fd) {
    const int flags = fcntl(fd, F_GETFL, 0);
    return flags >= 0 && fcntl(fd, F_SETFL, flags | O_NONBLOCK) == 0;
}

bool FillSendBuffer(int fd) {
    std::array<char, 4096> data = {};
    for (int i = 0; i < 1024; ++i) {
        const ssize_t written = send(fd, data.data(), data.size(), MSG_DONTWAIT | MSG_NOSIGNAL);
        if (written < 0) {
            return errno == EAGAIN;
        }
        if (written == 0) {
            return false;
        }
    }
    return false;
}

}  // namespace

TEST(UnixEpollReadiness, ConnectedSocketIsInitiallyWritable) {
    for (int type : {SOCK_STREAM, SOCK_SEQPACKET}) {
        int sockets[2] = {-1, -1};
        ASSERT_EQ(socketpair(AF_UNIX, type, 0, sockets), 0) << strerror(errno);
        const int epfd = epoll_create1(EPOLL_CLOEXEC);
        ASSERT_GE(epfd, 0);
        epoll_event event = {};
        event.events = EPOLLOUT | EPOLLET;
        ASSERT_EQ(epoll_ctl(epfd, EPOLL_CTL_ADD, sockets[0], &event), 0);
        uint32_t events = 0;
        ASSERT_EQ(WaitEvents(epfd, &events), 1);
        EXPECT_NE(events & EPOLLOUT, 0u);
        close(epfd);
        close(sockets[0]);
        close(sockets[1]);
    }
}

TEST(UnixEpollReadiness, ListenerWakesRdnormEdgeSubscriber) {
    const int listener = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
    ASSERT_GE(listener, 0);
    sockaddr_un address = {};
    address.sun_family = AF_UNIX;
    address.sun_path[0] = '\0';
    const int name_length = snprintf(address.sun_path + 1, sizeof(address.sun_path) - 1,
                                     "dkc014-epoll-%ld", static_cast<long>(getpid()));
    ASSERT_GT(name_length, 0);
    ASSERT_LT(static_cast<size_t>(name_length), sizeof(address.sun_path) - 1);
    const socklen_t length = offsetof(sockaddr_un, sun_path) + 1 + name_length;
    ASSERT_EQ(bind(listener, reinterpret_cast<sockaddr*>(&address), length), 0) << strerror(errno);
    ASSERT_EQ(listen(listener, 4), 0);

    const int epfd = epoll_create1(EPOLL_CLOEXEC);
    ASSERT_GE(epfd, 0);
    epoll_event event = {};
    event.events = EPOLLRDNORM | EPOLLET;
    ASSERT_EQ(epoll_ctl(epfd, EPOLL_CTL_ADD, listener, &event), 0);
    uint32_t events = 0;
    EXPECT_EQ(WaitEvents(epfd, &events, 0), 0);

    for (int round = 0; round < 2; ++round) {
        const int client = socket(AF_UNIX, SOCK_STREAM, 0);
        ASSERT_GE(client, 0);
        ASSERT_EQ(connect(client, reinterpret_cast<sockaddr*>(&address), length), 0)
            << strerror(errno);
        ASSERT_EQ(WaitEvents(epfd, &events), 1);
        EXPECT_NE(events & EPOLLRDNORM, 0u);
        const int accepted = accept4(listener, nullptr, nullptr, SOCK_NONBLOCK);
        ASSERT_GE(accepted, 0) << strerror(errno);
        close(accepted);
        close(client);
    }

    close(epfd);
    close(listener);
}

TEST(UnixEpollReadiness, WriteBandWakesAfterBufferDrain) {
    int sockets[2] = {-1, -1};
    ASSERT_EQ(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets), 0);
    ASSERT_TRUE(SetNonblocking(sockets[0]));
    ASSERT_TRUE(FillSendBuffer(sockets[0]));

    const int epfd = epoll_create1(EPOLL_CLOEXEC);
    ASSERT_GE(epfd, 0);
    epoll_event event = {};
    event.events = EPOLLWRBAND | EPOLLET;
    ASSERT_EQ(epoll_ctl(epfd, EPOLL_CTL_ADD, sockets[0], &event), 0);
    uint32_t events = 0;
    EXPECT_EQ(WaitEvents(epfd, &events, 0), 0);

    std::array<char, 4096> data = {};
    bool saw_write_band = false;
    for (int i = 0; i < 1024 && !saw_write_band; ++i) {
        ASSERT_GT(recv(sockets[1], data.data(), data.size(), MSG_DONTWAIT), 0);
        if (WaitEvents(epfd, &events, 0) == 1) {
            saw_write_band = (events & EPOLLWRBAND) != 0;
        }
    }
    EXPECT_TRUE(saw_write_band);
    close(epfd);
    close(sockets[0]);
    close(sockets[1]);
}

TEST(UnixEpollReadiness, WriteBandWakesAfterPeerClose) {
    int sockets[2] = {-1, -1};
    ASSERT_EQ(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets), 0);
    ASSERT_TRUE(SetNonblocking(sockets[0]));
    ASSERT_TRUE(FillSendBuffer(sockets[0]));
    const int epfd = epoll_create1(EPOLL_CLOEXEC);
    ASSERT_GE(epfd, 0);
    epoll_event event = {};
    event.events = EPOLLWRBAND | EPOLLET;
    ASSERT_EQ(epoll_ctl(epfd, EPOLL_CTL_ADD, sockets[0], &event), 0);
    uint32_t events = 0;
    EXPECT_EQ(WaitEvents(epfd, &events, 0), 0);

    close(sockets[1]);
    ASSERT_EQ(WaitEvents(epfd, &events), 1);
    EXPECT_NE(events & EPOLLWRBAND, 0u);
    close(epfd);
    close(sockets[0]);
}

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
