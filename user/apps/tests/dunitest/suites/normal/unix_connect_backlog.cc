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

enum class Release { Accept, Close, Grow, Timeout, Signal, SignalWithTimeout, LargeTimeout };

struct ConnectResult {
    int status;
    int error;
};

class ScopedFd {
public:
    explicit ScopedFd(int fd = -1) : fd_(fd) {}
    ~ScopedFd() {
        if (fd_ >= 0) close(fd_);
    }

    ScopedFd(const ScopedFd&) = delete;
    ScopedFd& operator=(const ScopedFd&) = delete;

    int get() const { return fd_; }

    void reset(int fd = -1) {
        if (fd_ >= 0) close(fd_);
        fd_ = fd;
    }

private:
    int fd_;
};

void RunFullBacklogConnect(Release release, int type) {
    sockaddr_un address{};
    address.sun_family = AF_UNIX;
    const int name_len = snprintf(address.sun_path + 1, sizeof(address.sun_path) - 1,
                                  "dkc033-%d-%d-%d", getpid(), type, static_cast<int>(release));
    ASSERT_GT(name_len, 0);
    const socklen_t addr_len = offsetof(sockaddr_un, sun_path) + 1 + name_len;

    const int listener = socket(AF_UNIX, type, 0);
    ASSERT_GE(listener, 0) << strerror(errno);
    ASSERT_EQ(0, bind(listener, reinterpret_cast<sockaddr*>(&address), addr_len))
        << strerror(errno);
    ASSERT_EQ(0, listen(listener, 0)) << strerror(errno);
    const int first = socket(AF_UNIX, type, 0);
    ASSERT_GE(first, 0);
    ASSERT_EQ(0, connect(first, reinterpret_cast<sockaddr*>(&address), addr_len))
        << strerror(errno);

    int ready_pipe[2];
    int result_pipe[2];
    ASSERT_EQ(0, pipe(ready_pipe));
    ASSERT_EQ(0, pipe(result_pipe));
    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        close(listener);
        close(first);
        close(ready_pipe[0]);
        close(result_pipe[0]);
        const int client = socket(AF_UNIX, type, 0);
        if (client < 0) _exit(2);
        if (release == Release::Timeout || release == Release::SignalWithTimeout ||
            release == Release::LargeTimeout) {
            const timeval timeout{release == Release::LargeTimeout ? 10000000000000LL : 0,
                                  release == Release::LargeTimeout ? 0 : 300000};
            if (setsockopt(client, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout)) != 0)
                _exit(3);
        }
        if (release == Release::Signal || release == Release::SignalWithTimeout) {
            struct sigaction action{};
            action.sa_handler = +[](int) {};
            if (release == Release::SignalWithTimeout) action.sa_flags = SA_RESTART;
            sigemptyset(&action.sa_mask);
            if (sigaction(SIGUSR1, &action, nullptr) != 0) _exit(4);
        }
        const char ready = 'r';
        if (write(ready_pipe[1], &ready, 1) != 1) _exit(5);
        errno = 0;
        const int status = connect(client, reinterpret_cast<sockaddr*>(&address), addr_len);
        const ConnectResult result{status, status == 0 ? 0 : errno};
        if (write(result_pipe[1], &result, sizeof(result)) != sizeof(result)) _exit(6);
        close(client);
        _exit(0);
    }

    close(ready_pipe[1]);
    close(result_pipe[1]);
    char ready = 0;
    EXPECT_EQ(1, read(ready_pipe[0], &ready, 1));
    close(ready_pipe[0]);
    pollfd poll_result{result_pipe[0], POLLIN, 0};
    EXPECT_EQ(0, poll(&poll_result, 1, 50)) << "connect did not block on the full backlog";

    if (release == Release::Accept || release == Release::LargeTimeout) {
        const int accepted = accept(listener, nullptr, nullptr);
        EXPECT_GE(accepted, 0) << strerror(errno);
        if (accepted >= 0) close(accepted);
    } else if (release == Release::Grow) {
        EXPECT_EQ(0, listen(listener, 1)) << strerror(errno);
    } else if (release == Release::Close) {
        close(listener);
    }

    poll_result.revents = 0;
    int poll_status = 0;
    if (release == Release::Signal || release == Release::SignalWithTimeout) {
        // The ready pipe precedes connect(2); resend until it actually reaches
        // the wait rather than relying on a scheduling-sensitive single signal.
        for (int attempt = 0; attempt < 150 && poll_status == 0; ++attempt) {
            if (kill(child, SIGUSR1) != 0 && errno != ESRCH) ADD_FAILURE() << strerror(errno);
            poll_status = poll(&poll_result, 1, 10);
        }
    } else {
        poll_status = poll(&poll_result, 1, 1500);
    }
    EXPECT_EQ(1, poll_status) << "blocked connect was not released";
    ConnectResult result{};
    if (poll_status == 1) {
        EXPECT_NE(0, poll_result.revents & POLLIN);
    }
    if (poll_status == 1 && (poll_result.revents & POLLIN)) {
        EXPECT_EQ(static_cast<ssize_t>(sizeof(result)),
                  read(result_pipe[0], &result, sizeof(result)));
        if (release == Release::Accept || release == Release::Grow ||
            release == Release::LargeTimeout) {
            EXPECT_EQ(0, result.status);
            if (result.status == 0) {
                // Each successful connect must have its own accept-ready entry.
                const int count = release == Release::Grow ? 2 : 1;
                for (int i = 0; i < count; ++i) {
                    pollfd ready{listener, POLLIN, 0};
                    const int ready_status = poll(&ready, 1, 5000);
                    EXPECT_EQ(1, ready_status) << "successful connect was not accept-ready";
                    if (ready_status != 1) break;
                    EXPECT_NE(0, ready.revents & POLLIN);
                    if (!(ready.revents & POLLIN)) break;
                    ScopedFd accepted(accept(listener, nullptr, nullptr));
                    EXPECT_GE(accepted.get(), 0) << strerror(errno);
                    if (accepted.get() < 0) break;
                }
                pollfd drained{listener, POLLIN, 0};
                EXPECT_EQ(0, poll(&drained, 1, 0)) << "unexpected extra accepted connection";
            }
        } else {
            EXPECT_EQ(-1, result.status);
            const int expected = release == Release::Close ? ECONNREFUSED
                                  : release == Release::Signal || release == Release::SignalWithTimeout
                                      ? EINTR
                                      : EAGAIN;
            EXPECT_EQ(expected, result.error);
        }
    }
    close(result_pipe[0]);
    if (poll_status != 1) kill(child, SIGKILL);
    int wait_status = 0;
    ASSERT_EQ(child, waitpid(child, &wait_status, 0));
    if (poll_status == 1) {
        EXPECT_TRUE(WIFEXITED(wait_status));
        EXPECT_EQ(0, WEXITSTATUS(wait_status));
    }
    close(first);
    if (release != Release::Close) close(listener);
}

class UnixConnectBacklog : public testing::TestWithParam<int> {};

enum class RaceRelease { Signal, SignalChain, Accept, Unlink, Rebind };

// Keep cleanup active even when an assertion aborts a race iteration.
struct ConnectRace {
    static constexpr int kMaxConnectors = 4;
    char path[sizeof(sockaddr_un::sun_path)]{};
    int listener = -1;
    int replacement = -1;
    int first = -1;
    int ready[kMaxConnectors][2]{{-1, -1}, {-1, -1}, {-1, -1}, {-1, -1}};
    int result[kMaxConnectors][2]{{-1, -1}, {-1, -1}, {-1, -1}, {-1, -1}};
    pid_t children[kMaxConnectors]{-1, -1, -1, -1};

    ~ConnectRace() {
        for (int i = 0; i < kMaxConnectors; ++i) {
            if (children[i] > 0) {
                kill(children[i], SIGKILL);
                while (waitpid(children[i], nullptr, 0) < 0 && errno == EINTR) {}
            }
            for (int j = 0; j < 2; ++j) {
                if (ready[i][j] >= 0) close(ready[i][j]);
                if (result[i][j] >= 0) close(result[i][j]);
            }
        }
        if (first >= 0) close(first);
        if (listener >= 0) close(listener);
        if (replacement >= 0) close(replacement);
        if (path[0]) unlink(path);
    }
};

void RunConnectorRace(int type, int iteration, RaceRelease release) {
    ConnectRace race;
    const int count = release == RaceRelease::Signal ? 2 : ConnectRace::kMaxConnectors;
    const int cancelled = release == RaceRelease::Signal ? 1
                          : release == RaceRelease::SignalChain ? count - 1 : 0;
    sockaddr_un address{};
    address.sun_family = AF_UNIX;
    const int name_len = snprintf(address.sun_path + 1, sizeof(address.sun_path) - 1,
                                  "backlog-race-%d-%d-%d", getpid(), type, iteration);
    ASSERT_GT(name_len, 0);
    socklen_t addr_len = offsetof(sockaddr_un, sun_path) + 1 + name_len;
    if (release == RaceRelease::Unlink || release == RaceRelease::Rebind) {
        snprintf(race.path, sizeof(race.path), "/tmp/backlog-race-%d-%d-%d",
                 getpid(), type, iteration);
        strcpy(address.sun_path, race.path);
        addr_len = offsetof(sockaddr_un, sun_path) + strlen(race.path) + 1;
    }
    race.listener = socket(AF_UNIX, type, 0);
    ASSERT_GE(race.listener, 0);
    ASSERT_EQ(0, bind(race.listener, reinterpret_cast<sockaddr*>(&address), addr_len));
    ASSERT_EQ(0, listen(race.listener, 0));
    race.first = socket(AF_UNIX, type, 0);
    ASSERT_GE(race.first, 0);
    ASSERT_EQ(0, connect(race.first, reinterpret_cast<sockaddr*>(&address), addr_len));

    for (int i = 0; i < count; ++i) {
        ASSERT_EQ(0, pipe(race.ready[i]));
        ASSERT_EQ(0, pipe(race.result[i]));
        race.children[i] = fork();
        ASSERT_GE(race.children[i], 0);
        if (race.children[i] == 0) {
            close(race.listener);
            close(race.first);
            close(race.ready[i][0]);
            close(race.result[i][0]);
            const int client = socket(AF_UNIX, type, 0);
            if (client < 0) _exit(2);
            struct sigaction action{};
            action.sa_handler = +[](int) {};
            sigemptyset(&action.sa_mask);
            // No SA_RESTART: signalled connectors must abandon their attempts.
            if (sigaction(SIGUSR1, &action, nullptr) != 0) _exit(3);
            const char ready = 'r';
            if (write(race.ready[i][1], &ready, 1) != 1) _exit(4);
            const int status = connect(client, reinterpret_cast<sockaddr*>(&address), addr_len);
            const ConnectResult result{status, status == 0 ? 0 : errno};
            if (write(race.result[i][1], &result, sizeof(result)) != sizeof(result)) _exit(5);
            close(client);
            _exit(0);
        }
        close(race.ready[i][1]);
        race.ready[i][1] = -1;
        close(race.result[i][1]);
        race.result[i][1] = -1;
        pollfd ready{race.ready[i][0], POLLIN, 0};
        ASSERT_EQ(1, poll(&ready, 1, 1500));
        char byte;
        ASSERT_EQ(1, read(race.ready[i][0], &byte, 1));
        pollfd result{race.result[i][0], POLLIN, 0};
        ASSERT_EQ(0, poll(&result, 1, 50)) << "connector " << i << " did not block";
    }

    // Race cancellation with releasing exactly one slot. In particular, accept
    // may select the first waiter before it processes the pending signal.
    for (int i = 0; i < cancelled; ++i) {
        ASSERT_EQ(0, kill(race.children[i], SIGUSR1));
    }
    if (release == RaceRelease::Unlink || release == RaceRelease::Rebind) {
        // A selected connector either fails re-resolution or moves to another
        // backlog. Both must pass the old listener's still-free capacity on.
        ASSERT_EQ(0, unlink(race.path));
        if (release == RaceRelease::Rebind) {
            race.replacement = socket(AF_UNIX, type | SOCK_NONBLOCK, 0);
            ASSERT_GE(race.replacement, 0);
            ASSERT_EQ(0, bind(race.replacement, reinterpret_cast<sockaddr*>(&address), addr_len));
            ASSERT_EQ(0, listen(race.replacement, 0));
        }
    }
    const int accepted = accept(race.listener, nullptr, nullptr);
    ASSERT_GE(accepted, 0);
    close(accepted);

    if (release == RaceRelease::Accept || release == RaceRelease::Rebind) {
        const int listener = release == RaceRelease::Rebind ? race.replacement : race.listener;
        // After each accept exactly one healthy connector can finish. Do not
        // require FIFO scheduling; poll all unfinished children together.
        for (int completed = 0; completed < count; ++completed) {
            pollfd results[ConnectRace::kMaxConnectors];
            for (int i = 0; i < count; ++i) {
                results[i] = {race.children[i] > 0 ? race.result[i][0] : -1, POLLIN, 0};
            }
            ASSERT_EQ(1, poll(results, count, 1500));
            for (int i = 0; i < count; ++i) {
                if (!(results[i].revents & POLLIN)) continue;
                ConnectResult result{};
                ASSERT_EQ(static_cast<ssize_t>(sizeof(result)),
                          read(results[i].fd, &result, sizeof(result)));
                EXPECT_EQ(0, result.status);
                EXPECT_EQ(0, result.error);
                int status;
                ASSERT_EQ(race.children[i], waitpid(race.children[i], &status, 0));
                race.children[i] = -1;
                EXPECT_TRUE(WIFEXITED(status));
                EXPECT_EQ(0, WEXITSTATUS(status));
                results[i].fd = -1;
            }
            ASSERT_EQ(0, poll(results, count, 50)) << "one slot admitted multiple connectors";
            if (completed + 1 < count) {
                const int next = accept(listener, nullptr, nullptr);
                ASSERT_GE(next, 0);
                close(next);
            }
        }
        return;
    }

    for (int i = 0; i < count; ++i) {
        pollfd ready{race.result[i][0], POLLIN, 0};
        ASSERT_EQ(1, poll(&ready, 1, 1500))
            << "connector " << i << " stalled after the only accept";
        ASSERT_NE(0, ready.revents & POLLIN);
        ConnectResult result{};
        ASSERT_EQ(static_cast<ssize_t>(sizeof(result)),
                  read(race.result[i][0], &result, sizeof(result)));
        const int expected = release == RaceRelease::Unlink ? ENOENT
                             : i < cancelled ? EINTR : 0;
        EXPECT_EQ(expected ? -1 : 0, result.status);
        EXPECT_EQ(expected, result.error);
        int status;
        ASSERT_EQ(race.children[i], waitpid(race.children[i], &status, 0));
        race.children[i] = -1;
        EXPECT_TRUE(WIFEXITED(status));
        EXPECT_EQ(0, WEXITSTATUS(status));
    }
}

TEST_P(UnixConnectBacklog, InterruptedConnectorDoesNotStrandNextConnector) {
    for (int iteration = 0; iteration < 32; ++iteration) {
        SCOPED_TRACE(iteration);
        RunConnectorRace(GetParam(), iteration, RaceRelease::Signal);
        if (HasFatalFailure()) return;
    }
}

TEST_P(UnixConnectBacklog, CancelledConnectorChainDoesNotStrandNextConnector) {
    for (int iteration = 0; iteration < 32; ++iteration) {
        SCOPED_TRACE(iteration);
        RunConnectorRace(GetParam(), iteration, RaceRelease::SignalChain);
        if (HasFatalFailure()) return;
    }
}

TEST_P(UnixConnectBacklog, OneAcceptAdmitsOneOfMultipleConnectors) {
    RunConnectorRace(GetParam(), 0, RaceRelease::Accept);
}

TEST_P(UnixConnectBacklog, AddressLookupFailurePassesWakeToNextConnector) {
    RunConnectorRace(GetParam(), 0, RaceRelease::Unlink);
}

TEST_P(UnixConnectBacklog, ReboundConnectorsPassOldBacklogWakeToNextConnector) {
    RunConnectorRace(GetParam(), 0, RaceRelease::Rebind);
}

TEST_P(UnixConnectBacklog, AcceptWakesBlockedConnector) {
    RunFullBacklogConnect(Release::Accept, GetParam());
}

TEST_P(UnixConnectBacklog, CloseWakesBlockedConnector) {
    RunFullBacklogConnect(Release::Close, GetParam());
}

TEST_P(UnixConnectBacklog, ListenGrowthWakesBlockedConnector) {
    RunFullBacklogConnect(Release::Grow, GetParam());
}

TEST_P(UnixConnectBacklog, SendTimeoutBoundsBlockedConnect) {
    RunFullBacklogConnect(Release::Timeout, GetParam());
}

TEST_P(UnixConnectBacklog, LargeFiniteTimeoutDoesNotOverflow) {
    RunFullBacklogConnect(Release::LargeTimeout, GetParam());
}

TEST_P(UnixConnectBacklog, SignalInterruptsBlockedConnect) {
    RunFullBacklogConnect(Release::Signal, GetParam());
}

TEST_P(UnixConnectBacklog, SignalInterruptsFiniteTimeoutDespiteSaRestart) {
    RunFullBacklogConnect(Release::SignalWithTimeout, GetParam());
}

TEST_P(UnixConnectBacklog, NonblockingFullBacklogReturnsEagain) {
    sockaddr_un address{};
    address.sun_family = AF_UNIX;
    const int name_len = snprintf(address.sun_path + 1, sizeof(address.sun_path) - 1,
                                  "dkc033-nonblock-%d-%d", getpid(), GetParam());
    ASSERT_GT(name_len, 0);
    const socklen_t addr_len = offsetof(sockaddr_un, sun_path) + 1 + name_len;
    const int listener = socket(AF_UNIX, GetParam(), 0);
    ASSERT_GE(listener, 0);
    ASSERT_EQ(0, bind(listener, reinterpret_cast<sockaddr*>(&address), addr_len));
    ASSERT_EQ(0, listen(listener, 0));
    const int first = socket(AF_UNIX, GetParam(), 0);
    ASSERT_GE(first, 0);
    ASSERT_EQ(0, connect(first, reinterpret_cast<sockaddr*>(&address), addr_len));
    const int second = socket(AF_UNIX, GetParam() | SOCK_NONBLOCK, 0);
    ASSERT_GE(second, 0);
    errno = 0;
    EXPECT_EQ(-1, connect(second, reinterpret_cast<sockaddr*>(&address), addr_len));
    EXPECT_EQ(EAGAIN, errno);
    close(second);
    close(first);
    close(listener);
}

TEST_P(UnixConnectBacklog, RepeatedConnectAcceptTransfersDataAndEof) {
    sockaddr_un address{};
    address.sun_family = AF_UNIX;
    const int name_len = snprintf(address.sun_path + 1, sizeof(address.sun_path) - 1,
                                  "dkc033-cycles-%d-%d", getpid(), GetParam());
    ASSERT_GT(name_len, 0);
    const socklen_t addr_len = offsetof(sockaddr_un, sun_path) + 1 + name_len;

    ScopedFd listener(socket(AF_UNIX, GetParam() | SOCK_NONBLOCK, 0));
    ASSERT_GE(listener.get(), 0) << strerror(errno);
    ASSERT_EQ(0, bind(listener.get(), reinterpret_cast<sockaddr*>(&address), addr_len))
        << strerror(errno);
    ASSERT_EQ(0, listen(listener.get(), 0)) << strerror(errno);

    for (int iteration = 0; iteration < 1024; ++iteration) {
        SCOPED_TRACE(iteration);
        ScopedFd client(socket(AF_UNIX, GetParam(), 0));
        ASSERT_GE(client.get(), 0) << strerror(errno);
        ASSERT_EQ(0, connect(client.get(), reinterpret_cast<sockaddr*>(&address), addr_len))
            << strerror(errno);

        pollfd ready{listener.get(), POLLIN, 0};
        ASSERT_EQ(1, poll(&ready, 1, 5000)) << "listener did not become accept-ready";
        ASSERT_NE(0, ready.revents & POLLIN);
        ScopedFd peer(accept(listener.get(), nullptr, nullptr));
        ASSERT_GE(peer.get(), 0) << strerror(errno);

        ASSERT_EQ(1, write(client.get(), "x", 1));
        char byte = 0;
        ASSERT_EQ(1, read(peer.get(), &byte, 1));
        EXPECT_EQ('x', byte);
        client.reset();
        ASSERT_EQ(0, read(peer.get(), &byte, 1));
    }
}

TEST_P(UnixConnectBacklog, ConnectedSocketReconnectReturnsEisconn) {
    sockaddr_un address{};
    address.sun_family = AF_UNIX;
    const int name_len = snprintf(address.sun_path + 1, sizeof(address.sun_path) - 1,
                                  "dkc033-eisconn-%d-%d", getpid(), GetParam());
    ASSERT_GT(name_len, 0);
    const socklen_t addr_len = offsetof(sockaddr_un, sun_path) + 1 + name_len;

    ScopedFd listener(socket(AF_UNIX, GetParam(), 0));
    ASSERT_GE(listener.get(), 0) << strerror(errno);
    ASSERT_EQ(0, bind(listener.get(), reinterpret_cast<sockaddr*>(&address), addr_len))
        << strerror(errno);
    ASSERT_EQ(0, listen(listener.get(), 0)) << strerror(errno);
    ScopedFd client(socket(AF_UNIX, GetParam(), 0));
    ASSERT_GE(client.get(), 0) << strerror(errno);
    ASSERT_EQ(0, connect(client.get(), reinterpret_cast<sockaddr*>(&address), addr_len))
        << strerror(errno);
    ScopedFd peer(accept(listener.get(), nullptr, nullptr));
    ASSERT_GE(peer.get(), 0) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, connect(client.get(), reinterpret_cast<sockaddr*>(&address), addr_len));
    EXPECT_EQ(EISCONN, errno);
}

INSTANTIATE_TEST_SUITE_P(StreamAndSeqpacket, UnixConnectBacklog,
                         testing::Values(SOCK_STREAM, SOCK_SEQPACKET));

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
