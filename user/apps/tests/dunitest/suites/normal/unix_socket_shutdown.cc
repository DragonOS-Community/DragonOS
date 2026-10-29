#include <gtest/gtest.h>

#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stddef.h>
#include <sys/socket.h>
#include <sys/un.h>
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

struct AbstractAddress {
    sockaddr_un addr{};
    socklen_t len = 0;
};

// Abstract namespace address: nothing to unlink, so a failing assertion cannot
// leave a stale socket file behind for the next case.
AbstractAddress MakeAbstractAddress(const char* tag) {
    AbstractAddress out;
    out.addr.sun_family = AF_UNIX;
    const int name_len = snprintf(out.addr.sun_path + 1, sizeof(out.addr.sun_path) - 1,
                                  "shutdown-%d-%s", getpid(), tag);
    EXPECT_GT(name_len, 0);
    out.len = static_cast<socklen_t>(offsetof(sockaddr_un, sun_path) + 1 + name_len);
    return out;
}

int MakeListener(const AbstractAddress& address, int backlog) {
    const int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    if (listener < 0) return -1;
    if (bind(listener, reinterpret_cast<const sockaddr*>(&address.addr), address.len) != 0 ||
        listen(listener, backlog) != 0) {
        close(listener);
        return -1;
    }
    return listener;
}

int ConnectTo(const AbstractAddress& address) {
    const int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    if (connect(fd, reinterpret_cast<const sockaddr*>(&address.addr), address.len) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

// `revents` of `fd` for exactly `events`, with no wait, or -1 when `poll(2)`
// itself fails. `poll(2)` reports `revents` filtered by `events` (plus the
// error bits), so a direction that is not requested cannot be observed.
short PollMaskWith(int fd, short events) {
    struct pollfd p = {};
    p.fd = fd;
    p.events = events;
    if (poll(&p, 1, 0) != 1) return -1;
    return p.revents;
}

// `revents` of `fd` with no wait, or -1 when `poll(2)` itself fails.
short PollMask(int fd) {
    return PollMaskWith(fd, POLLIN | POLLOUT | POLLRDHUP | POLLHUP);
}

// Whether `fd` reports both halves of the write trio `unix_poll()` derives
// together. They are only observable when requested explicitly, so the mask
// above cannot catch a missing `POLLWRNORM`/`POLLWRBAND`.
bool WriteTrioComplete(int fd) {
    return (PollMaskWith(fd, POLLWRNORM) & POLLWRNORM) != 0 &&
           (PollMaskWith(fd, POLLWRBAND) & POLLWRBAND) != 0;
}

// Wait until `fd` reports one of `events`.
//
// Blocking assertions are bounded on purpose: a regression that stops waking a
// waiter would otherwise hang until the runner kills the process after its 60s
// budget, which discards the result of every other case in this suite as well.
bool WaitForEvents(int fd, short events) {
    struct pollfd p = {};
    p.fd = fd;
    p.events = events;
    return poll(&p, 1, 2'000) == 1 && (p.revents & events) != 0;
}

// `accept(2)` only after the listener announced a pending connection, so a lost
// wakeup shows up as a failed assertion instead of a hang.
int AcceptWithDeadline(int listener) {
    if (!WaitForEvents(listener, POLLIN | POLLRDHUP)) return -1;
    return accept(listener, nullptr, nullptr);
}

// `read(2)` with a bounded wait; returns -2 when nothing arrived in time.
ssize_t ReadWithDeadline(int fd, char* buf, size_t len) {
    if (!WaitForEvents(fd, POLLIN | POLLRDHUP)) return -2;
    return read(fd, buf, len);
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

// `unix_shutdown()` never looks at the connection state: a socket that is
// neither connected nor listening latches `sk_shutdown` and reports success,
// for every direction and repeatedly. Only an out-of-range `how` is rejected.
TEST(UnixSocketShutdown, UnconnectedShutdownReportsSuccess) {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GE(fd, 0) << std::strerror(errno);

    EXPECT_EQ(shutdown(fd, SHUT_WR), 0) << std::strerror(errno);
    EXPECT_EQ(shutdown(fd, SHUT_RD), 0) << std::strerror(errno);
    EXPECT_EQ(shutdown(fd, SHUT_RDWR), 0) << std::strerror(errno);
    // Sticky: the bits only accumulate, so repeating a direction succeeds.
    EXPECT_EQ(shutdown(fd, SHUT_RD), 0) << std::strerror(errno);
    close(fd);

    const AbstractAddress address = MakeAbstractAddress("bound");
    fd = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GE(fd, 0) << std::strerror(errno);
    ASSERT_EQ(bind(fd, reinterpret_cast<const sockaddr*>(&address.addr), address.len), 0)
        << std::strerror(errno);
    EXPECT_EQ(shutdown(fd, SHUT_RD), 0) << std::strerror(errno);
    close(fd);

    fd = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GE(fd, 0) << std::strerror(errno);
    errno = 0;
    const int bad_high = shutdown(fd, 3);
    const int bad_high_errno = errno;
    EXPECT_EQ(bad_high, -1);
    EXPECT_EQ(bad_high_errno, EINVAL);
    errno = 0;
    const int bad_low = shutdown(fd, -1);
    const int bad_low_errno = errno;
    EXPECT_EQ(bad_low, -1);
    EXPECT_EQ(bad_low_errno, EINVAL);
    close(fd);
}

// The poll mask of an unconnected socket gains what `unix_poll()` derives from
// the latch (`RCV_SHUTDOWN` -> readable + half-closed) without losing the
// `EPOLLHUP | EPOLLOUT` an unconnected stream socket always reports. `SHUT_WR`
// adds nothing, because only the receive direction is observable.
TEST(UnixSocketShutdown, UnconnectedShutdownReportsPollState) {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GE(fd, 0) << std::strerror(errno);

    EXPECT_NE(PollMask(fd), -1);
    EXPECT_EQ(PollMask(fd) & (POLLHUP | POLLOUT), POLLHUP | POLLOUT);
    EXPECT_EQ(PollMask(fd) & (POLLIN | POLLRDHUP), 0);
    // A stream socket is writable in `TCP_CLOSE` (`unix_writable()` only looks
    // at the state), and the trio is one mask, so both extra bits must be there.
    EXPECT_TRUE(WriteTrioComplete(fd));

    ASSERT_EQ(shutdown(fd, SHUT_WR), 0) << std::strerror(errno);
    // The send direction is not observable through `poll(2)` at all, so the
    // whole mask must stay exactly what it was.
    EXPECT_EQ(PollMask(fd) & (POLLIN | POLLRDHUP), 0);
    EXPECT_EQ(PollMask(fd) & (POLLHUP | POLLOUT), POLLHUP | POLLOUT);
    EXPECT_TRUE(WriteTrioComplete(fd));

    ASSERT_EQ(shutdown(fd, SHUT_RD), 0) << std::strerror(errno);
    EXPECT_EQ(PollMask(fd) & (POLLIN | POLLRDHUP | POLLHUP | POLLOUT),
              POLLIN | POLLRDHUP | POLLHUP | POLLOUT);
    EXPECT_TRUE(WriteTrioComplete(fd));
    close(fd);
}

// A `SHUT_RD` latched before `connect(2)` survives the transition, because
// Linux keeps `sk_shutdown` on the socket itself: the client is at EOF right
// away and the peer's `send(2)` fails with `EPIPE`, which is the
// `SEND_SHUTDOWN` that `unix_shutdown()` propagated to the other end.
TEST(UnixSocketShutdown, UnconnectedReceiveShutdownSurvivesConnect) {
    const AbstractAddress address = MakeAbstractAddress("connect");
    const int listener = MakeListener(address, 4);
    ASSERT_GE(listener, 0) << std::strerror(errno);

    const int client = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GE(client, 0) << std::strerror(errno);
    ASSERT_EQ(shutdown(client, SHUT_RD), 0) << std::strerror(errno);
    ASSERT_EQ(connect(client, reinterpret_cast<const sockaddr*>(&address.addr), address.len), 0)
        << std::strerror(errno);

    const int server = AcceptWithDeadline(listener);
    ASSERT_GE(server, 0) << std::strerror(errno);

    EXPECT_NE(PollMask(client) & POLLIN, 0);
    EXPECT_NE(PollMask(client) & POLLRDHUP, 0);

    errno = 0;
    const ssize_t sent = send(server, "x", 1, MSG_NOSIGNAL);
    const int send_errno = errno;
    EXPECT_EQ(sent, -1);
    EXPECT_EQ(send_errno, EPIPE);

    char byte = 0;
    EXPECT_EQ(ReadWithDeadline(client, &byte, sizeof(byte)), 0) << std::strerror(errno);

    close(server);
    close(client);
    close(listener);
}

// A `SHUT_RD` latched before `listen(2)` is inherited by the listener, so it
// already refuses new connectors and fails `accept(2)` - the same contract a
// listener shut down after `listen(2)` has (see `unix_accept_close.cc`).
TEST(UnixSocketShutdown, UnconnectedReceiveShutdownSurvivesListen) {
    const AbstractAddress address = MakeAbstractAddress("listen");
    const int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GE(listener, 0) << std::strerror(errno);
    ASSERT_EQ(bind(listener, reinterpret_cast<const sockaddr*>(&address.addr), address.len), 0)
        << std::strerror(errno);
    ASSERT_EQ(shutdown(listener, SHUT_RD), 0) << std::strerror(errno);
    ASSERT_EQ(listen(listener, 4), 0) << std::strerror(errno);

    errno = 0;
    const int accepted = AcceptWithDeadline(listener);
    const int accept_errno = errno;
    EXPECT_EQ(accepted, -1);
    EXPECT_EQ(accept_errno, EINVAL);

    errno = 0;
    const int connector = ConnectTo(address);
    const int connect_errno = errno;
    EXPECT_EQ(connector, -1);
    EXPECT_EQ(connect_errno, ECONNREFUSED);

    close(listener);
}

// Only the receive direction refuses connections: a `SHUT_WR` latched before
// `listen(2)` must leave the listener usable.
TEST(UnixSocketShutdown, UnconnectedSendShutdownKeepsListenerUsable) {
    const AbstractAddress address = MakeAbstractAddress("listen-wr");
    const int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GE(listener, 0) << std::strerror(errno);
    ASSERT_EQ(bind(listener, reinterpret_cast<const sockaddr*>(&address.addr), address.len), 0)
        << std::strerror(errno);
    ASSERT_EQ(shutdown(listener, SHUT_WR), 0) << std::strerror(errno);
    ASSERT_EQ(listen(listener, 4), 0) << std::strerror(errno);

    const int client = ConnectTo(address);
    ASSERT_GE(client, 0) << std::strerror(errno);
    const int server = AcceptWithDeadline(listener);
    ASSERT_GE(server, 0) << std::strerror(errno);

    EXPECT_EQ(send(client, "x", 1, MSG_NOSIGNAL), 1) << std::strerror(errno);

    close(server);
    close(client);
    close(listener);
}

// `SHUT_RDWR` latched before `connect(2)`: both halves survive, but only the
// receive half reaches the peer. Linux keeps the send half on the socket that
// latched it, because `unix_shutdown()` mirrors a direction onto the peer only
// when the socket already has one - a socket that is not connected yet has
// none. So the peer keeps blocking on `read(2)`.
TEST(UnixSocketShutdown, UnconnectedReadWriteShutdownSurvivesConnect) {
    const AbstractAddress address = MakeAbstractAddress("connect-rdwr");
    const int listener = MakeListener(address, 4);
    ASSERT_GE(listener, 0) << std::strerror(errno);

    const int client = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GE(client, 0) << std::strerror(errno);
    ASSERT_EQ(shutdown(client, SHUT_RDWR), 0) << std::strerror(errno);
    ASSERT_EQ(connect(client, reinterpret_cast<const sockaddr*>(&address.addr), address.len), 0)
        << std::strerror(errno);

    const int server = AcceptWithDeadline(listener);
    ASSERT_GE(server, 0) << std::strerror(errno);

    // The socket that latched the bits reports both directions closed.
    const short client_mask = PollMask(client);
    ASSERT_NE(client_mask, -1);
    EXPECT_NE(client_mask & POLLIN, 0);
    EXPECT_NE(client_mask & POLLRDHUP, 0);
    EXPECT_NE(client_mask & POLLHUP, 0);

    // The peer sees a plain, healthy connection.
    const short server_mask = PollMask(server);
    ASSERT_NE(server_mask, -1);
    EXPECT_EQ(server_mask & (POLLIN | POLLRDHUP | POLLHUP), 0);

    char byte = 0;
    errno = 0;
    const ssize_t peeked = recv(server, &byte, sizeof(byte), MSG_DONTWAIT);
    const int peek_errno = errno;
    EXPECT_EQ(peeked, -1);
    EXPECT_EQ(peek_errno, EAGAIN);

    errno = 0;
    const ssize_t from_client = write(client, "x", 1);
    const int write_errno = errno;
    EXPECT_EQ(from_client, -1);
    EXPECT_EQ(write_errno, EPIPE);

    errno = 0;
    const ssize_t from_server = send(server, "y", 1, MSG_NOSIGNAL);
    const int send_errno = errno;
    EXPECT_EQ(from_server, -1);
    EXPECT_EQ(send_errno, EPIPE);

    EXPECT_EQ(ReadWithDeadline(client, &byte, sizeof(byte)), 0) << std::strerror(errno);

    close(server);
    close(client);
    close(listener);
}

// A `SHUT_WR` latched before `connect(2)` closes only the writer that latched
// it. Linux never turns it into an EOF for the peer, so the peer must still be
// able to read (and block) and to write.
TEST(UnixSocketShutdown, UnconnectedSendShutdownDoesNotEofPeerAfterConnect) {
    const AbstractAddress address = MakeAbstractAddress("connect-wr");
    const int listener = MakeListener(address, 4);
    ASSERT_GE(listener, 0) << std::strerror(errno);

    const int client = socket(AF_UNIX, SOCK_STREAM, 0);
    ASSERT_GE(client, 0) << std::strerror(errno);
    ASSERT_EQ(shutdown(client, SHUT_WR), 0) << std::strerror(errno);
    ASSERT_EQ(connect(client, reinterpret_cast<const sockaddr*>(&address.addr), address.len), 0)
        << std::strerror(errno);

    const int server = AcceptWithDeadline(listener);
    ASSERT_GE(server, 0) << std::strerror(errno);

    // The writer is closed locally: `EPIPE`, and no `POLLHUP` for a half-close.
    const short client_mask = PollMask(client);
    ASSERT_NE(client_mask, -1);
    EXPECT_NE(client_mask & POLLOUT, 0);
    EXPECT_EQ(client_mask & (POLLIN | POLLRDHUP | POLLHUP), 0);

    // The peer is untouched: no data, no EOF, still writable.
    const short server_mask = PollMask(server);
    ASSERT_NE(server_mask, -1);
    EXPECT_EQ(server_mask & (POLLIN | POLLRDHUP | POLLHUP), 0);

    char byte = 0;
    errno = 0;
    const ssize_t peeked = recv(server, &byte, sizeof(byte), MSG_DONTWAIT);
    const int peek_errno = errno;
    EXPECT_EQ(peeked, -1);
    EXPECT_EQ(peek_errno, EAGAIN);

    errno = 0;
    const ssize_t rejected = write(client, "x", 1);
    const int rejected_errno = errno;
    EXPECT_EQ(rejected, -1);
    EXPECT_EQ(rejected_errno, EPIPE);

    EXPECT_EQ(send(server, "y", 1, MSG_NOSIGNAL), 1) << std::strerror(errno);

    close(server);
    close(client);
    close(listener);
}

int main(int argc, char** argv) {
    // Several cases assert `send(2)` failures on purpose; a stray `SIGPIPE`
    // would take down the whole binary and hide their results.
    signal(SIGPIPE, SIG_IGN);
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
