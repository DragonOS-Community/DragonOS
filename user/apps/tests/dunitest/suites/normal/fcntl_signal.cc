#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <unistd.h>

#ifndef POLL_IN
#define POLL_IN 1
#endif

#ifndef POLL_OUT
#define POLL_OUT 2
#endif

#ifndef POLL_HUP
#define POLL_HUP 6
#endif

#ifndef F_SETSIG
#define F_SETSIG 10
#endif

#ifndef F_GETSIG
#define F_GETSIG 11
#endif

namespace {

constexpr int kKernelSigRtMax = 64;

volatile sig_atomic_t g_signal_count = 0;
volatile sig_atomic_t g_signal_number = 0;
volatile sig_atomic_t g_signal_fd = -1;
volatile sig_atomic_t g_signal_code = 0;
volatile sig_atomic_t g_signal_band = 0;

void reset_signal_state() {
    g_signal_count = 0;
    g_signal_number = 0;
    g_signal_fd = -1;
    g_signal_code = 0;
    g_signal_band = 0;
}

void fasync_signal_handler(int sig, siginfo_t* info, void*) {
    g_signal_count++;
    g_signal_number = sig;
    if (info != nullptr) {
        g_signal_fd = info->si_fd;
        g_signal_code = info->si_code;
        g_signal_band = info->si_band;
    }
}

void install_signal_handler(int signum) {
    struct sigaction action = {};
    action.sa_sigaction = fasync_signal_handler;
    sigemptyset(&action.sa_mask);
    action.sa_flags = SA_SIGINFO;
    ASSERT_EQ(0, sigaction(signum, &action, nullptr))
        << "sigaction failed: errno=" << errno << " (" << strerror(errno) << ")";
}

bool wait_for_signal() {
    for (int i = 0; i < 100; ++i) {
        if (g_signal_count > 0) {
            return true;
        }
        usleep(10 * 1000);
    }
    return false;
}

}  // namespace

TEST(FcntlSignal, GetSigDefaultsToZero) {
    int fds[2] = {-1, -1};
    ASSERT_EQ(0, pipe(fds));

    EXPECT_EQ(0, fcntl(fds[0], F_GETSIG));

    close(fds[0]);
    close(fds[1]);
}

TEST(FcntlSignal, SetSigRoundTripAndValidation) {
    int fds[2] = {-1, -1};
    ASSERT_EQ(0, pipe(fds));

    ASSERT_EQ(0, fcntl(fds[0], F_SETSIG, SIGUSR1));
    EXPECT_EQ(SIGUSR1, fcntl(fds[0], F_GETSIG));

    errno = 0;
    EXPECT_EQ(-1, fcntl(fds[0], F_SETSIG, kKernelSigRtMax + 1));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(SIGUSR1, fcntl(fds[0], F_GETSIG));

    ASSERT_EQ(0, fcntl(fds[0], F_SETSIG, 0));
    EXPECT_EQ(0, fcntl(fds[0], F_GETSIG));

    close(fds[0]);
    close(fds[1]);
}

TEST(FcntlSignal, CustomSignalDeliveredForPipeAsyncIo) {
    int fds[2] = {-1, -1};
    ASSERT_EQ(0, pipe(fds));

    install_signal_handler(SIGUSR1);
    reset_signal_state();

    ASSERT_EQ(0, fcntl(fds[0], F_SETOWN, getpid()));
    ASSERT_EQ(0, fcntl(fds[0], F_SETSIG, SIGUSR1));

    int flags = fcntl(fds[0], F_GETFL);
    ASSERT_GE(flags, 0);
    ASSERT_EQ(0, fcntl(fds[0], F_SETFL, flags | O_ASYNC));

    ASSERT_EQ(1, write(fds[1], "x", 1));
    ASSERT_TRUE(wait_for_signal());

    EXPECT_EQ(SIGUSR1, g_signal_number);
    EXPECT_EQ(fds[0], g_signal_fd);
    EXPECT_EQ(POLL_IN, g_signal_code);
    EXPECT_EQ(EPOLLIN | EPOLLRDNORM, g_signal_band);

    close(fds[0]);
    close(fds[1]);
}

TEST(FcntlSignal, PipeCloseReportsLinuxBands) {
    int fds[2] = {-1, -1};
    ASSERT_EQ(0, pipe(fds));

    install_signal_handler(SIGUSR1);
    reset_signal_state();

    ASSERT_EQ(0, fcntl(fds[0], F_SETOWN, getpid()));
    ASSERT_EQ(0, fcntl(fds[0], F_SETSIG, SIGUSR1));

    int read_flags = fcntl(fds[0], F_GETFL);
    ASSERT_GE(read_flags, 0);
    ASSERT_EQ(0, fcntl(fds[0], F_SETFL, read_flags | O_ASYNC));

    ASSERT_EQ(0, close(fds[1]));
    ASSERT_TRUE(wait_for_signal());

    EXPECT_EQ(SIGUSR1, g_signal_number);
    EXPECT_EQ(fds[0], g_signal_fd);
    EXPECT_EQ(POLL_IN, g_signal_code);
    EXPECT_EQ(EPOLLIN | EPOLLRDNORM, g_signal_band);

    ASSERT_EQ(0, close(fds[0]));

    ASSERT_EQ(0, pipe(fds));
    reset_signal_state();

    ASSERT_EQ(0, fcntl(fds[1], F_SETOWN, getpid()));
    ASSERT_EQ(0, fcntl(fds[1], F_SETSIG, SIGUSR1));

    int write_flags = fcntl(fds[1], F_GETFL);
    ASSERT_GE(write_flags, 0);
    ASSERT_EQ(0, fcntl(fds[1], F_SETFL, write_flags | O_ASYNC));

    ASSERT_EQ(0, close(fds[0]));
    ASSERT_TRUE(wait_for_signal());

    EXPECT_EQ(SIGUSR1, g_signal_number);
    EXPECT_EQ(fds[1], g_signal_fd);
    EXPECT_EQ(POLL_OUT, g_signal_code);
    EXPECT_EQ(EPOLLOUT | EPOLLWRNORM | EPOLLWRBAND, g_signal_band);

    close(fds[1]);
}

TEST(FcntlSignal, UnixStreamPeerCloseReportsPollHup) {
    int fds[2] = {-1, -1};
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));

    install_signal_handler(SIGUSR1);
    reset_signal_state();

    ASSERT_EQ(0, fcntl(fds[0], F_SETOWN, getpid()));
    ASSERT_EQ(0, fcntl(fds[0], F_SETSIG, SIGUSR1));

    int flags = fcntl(fds[0], F_GETFL);
    ASSERT_GE(flags, 0);
    ASSERT_EQ(0, fcntl(fds[0], F_SETFL, flags | O_ASYNC));

    ASSERT_EQ(0, close(fds[1]));
    ASSERT_TRUE(wait_for_signal());

    EXPECT_EQ(SIGUSR1, g_signal_number);
    EXPECT_EQ(fds[0], g_signal_fd);
    EXPECT_EQ(POLL_HUP, g_signal_code);
    EXPECT_EQ(EPOLLHUP | EPOLLERR, g_signal_band);

    close(fds[0]);
}

TEST(FcntlSignal, DupKeepsRegisteredFdForSiginfo) {
    int fds[2] = {-1, -1};
    ASSERT_EQ(0, pipe(fds));

    int dup_fd = dup(fds[0]);
    ASSERT_GE(dup_fd, 0);

    install_signal_handler(SIGUSR2);
    reset_signal_state();

    ASSERT_EQ(0, fcntl(fds[0], F_SETOWN, getpid()));
    ASSERT_EQ(0, fcntl(fds[0], F_SETSIG, SIGUSR1));

    int flags = fcntl(fds[0], F_GETFL);
    ASSERT_GE(flags, 0);
    ASSERT_EQ(0, fcntl(fds[0], F_SETFL, flags | O_ASYNC));

    ASSERT_EQ(0, fcntl(dup_fd, F_SETSIG, SIGUSR2));
    ASSERT_EQ(1, write(fds[1], "x", 1));
    ASSERT_TRUE(wait_for_signal());

    EXPECT_EQ(SIGUSR2, g_signal_number);
    EXPECT_EQ(fds[0], g_signal_fd);
    EXPECT_EQ(POLL_IN, g_signal_code);
    EXPECT_EQ(EPOLLIN | EPOLLRDNORM, g_signal_band);

    close(dup_fd);
    close(fds[0]);
    close(fds[1]);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
