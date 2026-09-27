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

INSTANTIATE_TEST_SUITE_P(StreamAndSeqpacket, UnixConnectBacklog,
                         testing::Values(SOCK_STREAM, SOCK_SEQPACKET));

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
