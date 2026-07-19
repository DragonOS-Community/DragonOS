#include <gtest/gtest.h>

#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#include <chrono>
#include <cstdio>
#include <cstring>

namespace {

bool WaitForSleepingProcess(pid_t pid) {
    char path[64] = {};
    std::snprintf(path, sizeof(path), "/proc/%d/stat", pid);
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(500);
    while (std::chrono::steady_clock::now() < deadline) {
        FILE* stat = std::fopen(path, "r");
        if (stat != nullptr) {
            char line[512] = {};
            const bool read = std::fgets(line, sizeof(line), stat) != nullptr;
            std::fclose(stat);
            if (read) {
                const char* comm_end = std::strrchr(line, ')');
                if (comm_end != nullptr && comm_end[1] == ' ' &&
                    (comm_end[2] == 'S' || comm_end[2] == 'D')) {
                    return true;
                }
            }
        }
        usleep(1'000);
    }
    return false;
}

void ExpectPeerReadWokenByShutdown(int socket_type) {
    int sockets[2] = {-1, -1};
    int ready[2] = {-1, -1};
    ASSERT_EQ(socketpair(AF_UNIX, socket_type, 0, sockets), 0) << std::strerror(errno);
    ASSERT_EQ(pipe(ready), 0) << std::strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << std::strerror(errno);
    if (child == 0) {
        close(sockets[0]);
        close(ready[0]);

        struct timeval timeout = {};
        timeout.tv_sec = 1;
        if (setsockopt(sockets[1], SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) != 0) {
            _exit(2);
        }
        char marker = 'R';
        if (write(ready[1], &marker, sizeof(marker)) != sizeof(marker)) {
            _exit(3);
        }

        const auto start = std::chrono::steady_clock::now();
        char byte = 0;
        const ssize_t nread = read(sockets[1], &byte, sizeof(byte));
        const auto elapsed = std::chrono::steady_clock::now() - start;
        if (nread != 0 || elapsed >= std::chrono::milliseconds(800)) {
            _exit(4);
        }
        _exit(0);
    }

    close(sockets[1]);
    close(ready[1]);
    char marker = 0;
    ASSERT_EQ(read(ready[0], &marker, sizeof(marker)), 1);
    ASSERT_EQ(marker, 'R');
    if (!WaitForSleepingProcess(child)) {
        kill(child, SIGKILL);
        waitpid(child, nullptr, 0);
        FAIL() << "child did not block in read";
    }
    ASSERT_EQ(shutdown(sockets[0], SHUT_WR), 0) << std::strerror(errno);

    int status = 0;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
    pid_t waited = 0;
    while (std::chrono::steady_clock::now() < deadline) {
        waited = waitpid(child, &status, WNOHANG);
        if (waited != 0) {
            break;
        }
        usleep(1'000);
    }
    if (waited == 0) {
        kill(child, SIGKILL);
        waitpid(child, &status, 0);
        FAIL() << "peer read did not wake after shutdown(SHUT_WR)";
    }
    ASSERT_EQ(waited, child) << std::strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);

    close(sockets[0]);
    close(ready[0]);
}

void ExpectLocalReadShutdownDrainsThenReturnsEof(int socket_type) {
    int sockets[2] = {-1, -1};
    ASSERT_EQ(socketpair(AF_UNIX, socket_type, 0, sockets), 0) << std::strerror(errno);

    constexpr char payload[] = "abc";
    ASSERT_EQ(send(sockets[1], payload, sizeof(payload) - 1, 0),
              static_cast<ssize_t>(sizeof(payload) - 1))
        << std::strerror(errno);
    ASSERT_EQ(shutdown(sockets[0], SHUT_RD), 0) << std::strerror(errno);

    struct pollfd local = {};
    local.fd = sockets[0];
    local.events = POLLIN | POLLRDHUP;
    ASSERT_EQ(poll(&local, 1, 0), 1) << std::strerror(errno);
    EXPECT_NE(local.revents & POLLIN, 0);
    EXPECT_NE(local.revents & POLLRDHUP, 0);

    char received[sizeof(payload)] = {};
    ASSERT_EQ(recv(sockets[0], received, sizeof(received), 0),
              static_cast<ssize_t>(sizeof(payload) - 1))
        << std::strerror(errno);
    EXPECT_EQ(std::memcmp(received, payload, sizeof(payload) - 1), 0);

    // Once pre-shutdown data is drained, SHUT_RD is an immediate EOF rather
    // than EAGAIN or another blocking wait.
    EXPECT_EQ(recv(sockets[0], received, sizeof(received), MSG_PEEK | MSG_DONTWAIT), 0)
        << std::strerror(errno);

    struct iovec iov = {};
    iov.iov_base = received;
    iov.iov_len = sizeof(received);
    struct msghdr msg = {};
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    EXPECT_EQ(recvmsg(sockets[0], &msg, MSG_DONTWAIT), 0) << std::strerror(errno);

    EXPECT_EQ(recv(sockets[0], received, sizeof(received), MSG_DONTWAIT), 0)
        << std::strerror(errno);

    errno = 0;
    EXPECT_EQ(send(sockets[1], payload, sizeof(payload) - 1, MSG_NOSIGNAL), -1);
    EXPECT_EQ(errno, EPIPE);

    close(sockets[0]);
    close(sockets[1]);
}

void ExpectLocalReadWokenByShutdown(int socket_type) {
    int sockets[2] = {-1, -1};
    int ready[2] = {-1, -1};
    ASSERT_EQ(socketpair(AF_UNIX, socket_type, 0, sockets), 0) << std::strerror(errno);
    ASSERT_EQ(pipe(ready), 0) << std::strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << std::strerror(errno);
    if (child == 0) {
        close(sockets[1]);
        close(ready[0]);
        struct timeval timeout = {};
        timeout.tv_sec = 1;
        if (setsockopt(sockets[0], SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) != 0) {
            _exit(2);
        }
        char marker = 'R';
        if (write(ready[1], &marker, sizeof(marker)) != sizeof(marker)) {
            _exit(3);
        }
        const auto start = std::chrono::steady_clock::now();
        char byte = 0;
        const ssize_t nread = read(sockets[0], &byte, sizeof(byte));
        const auto elapsed = std::chrono::steady_clock::now() - start;
        _exit(nread == 0 && elapsed < std::chrono::milliseconds(800) ? 0 : 4);
    }

    close(ready[1]);
    char marker = 0;
    ASSERT_EQ(read(ready[0], &marker, sizeof(marker)), 1);
    ASSERT_EQ(marker, 'R');
    if (!WaitForSleepingProcess(child)) {
        kill(child, SIGKILL);
        waitpid(child, nullptr, 0);
        FAIL() << "child did not block in local read";
    }

    ASSERT_EQ(shutdown(sockets[0], SHUT_RD), 0) << std::strerror(errno);
    int status = 0;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
    pid_t waited = 0;
    while (std::chrono::steady_clock::now() < deadline) {
        waited = waitpid(child, &status, WNOHANG);
        if (waited != 0) {
            break;
        }
        usleep(1'000);
    }
    if (waited == 0) {
        kill(child, SIGKILL);
        waitpid(child, &status, 0);
        FAIL() << "local read did not wake after shutdown(SHUT_RD)";
    }
    ASSERT_EQ(waited, child) << std::strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);

    close(sockets[0]);
    close(sockets[1]);
    close(ready[0]);
}

}  // namespace

TEST(UnixSocketShutdown, StreamPeerReadWakesForEof) {
    ExpectPeerReadWokenByShutdown(SOCK_STREAM);
}

TEST(UnixSocketShutdown, SeqPacketPeerReadWakesForEof) {
    ExpectPeerReadWokenByShutdown(SOCK_SEQPACKET);
}

TEST(UnixSocketShutdown, StreamLocalReadShutdownDrainsThenReturnsEof) {
    ExpectLocalReadShutdownDrainsThenReturnsEof(SOCK_STREAM);
}

TEST(UnixSocketShutdown, SeqPacketLocalReadShutdownDrainsThenReturnsEof) {
    ExpectLocalReadShutdownDrainsThenReturnsEof(SOCK_SEQPACKET);
}

TEST(UnixSocketShutdown, StreamLocalReadWakesForEof) {
    ExpectLocalReadWokenByShutdown(SOCK_STREAM);
}

TEST(UnixSocketShutdown, SeqPacketLocalReadWakesForEof) {
    ExpectLocalReadWokenByShutdown(SOCK_SEQPACKET);
}

TEST(UnixSocketShutdown, PollReportsDirectionalAndFullShutdown) {
    int sockets[2] = {-1, -1};
    ASSERT_EQ(socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sockets), 0) << std::strerror(errno);

    ASSERT_EQ(shutdown(sockets[0], SHUT_WR), 0) << std::strerror(errno);
    struct pollfd peer = {};
    peer.fd = sockets[1];
    peer.events = POLLIN | POLLRDHUP;
    ASSERT_EQ(poll(&peer, 1, 0), 1) << std::strerror(errno);
    EXPECT_NE(peer.revents & POLLIN, 0);
    EXPECT_NE(peer.revents & POLLRDHUP, 0);

    ASSERT_EQ(shutdown(sockets[0], SHUT_RD), 0) << std::strerror(errno);
    peer.revents = 0;
    ASSERT_EQ(poll(&peer, 1, 0), 1) << std::strerror(errno);
    EXPECT_NE(peer.revents & POLLHUP, 0);

    close(sockets[0]);
    close(sockets[1]);
}

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
