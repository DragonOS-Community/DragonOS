#include <gtest/gtest.h>

#include <errno.h>
#include <limits.h>
#include <signal.h>
#include <stdint.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <vector>

namespace {
// The kernel ABI uses an eight-byte signal set, not libc's sigset_t.
constexpr size_t kSigsetSize = sizeof(uint64_t);
int pwait2(int fd, epoll_event* events, int count, const timespec* timeout,
           const uint64_t* mask = nullptr, size_t size = kSigsetSize) {
    return syscall(441, fd, events, count, timeout, mask, size);
}
volatile sig_atomic_t received = 0;
void on_signal(int) { received = 1; }

class EpollPwait2 : public ::testing::Test {
protected:
    void SetUp() override {
        // Bound even NULL/huge timeout regressions. The runner records a killed
        // test as a failure; no signal handler can silently turn it into a pass.
        alarm(15);
        epfd = epoll_create1(0);
        ASSERT_GE(epfd, 0);
    }
    void TearDown() override {
        if (child > 0) {
            kill(child, SIGKILL);
            waitpid(child, nullptr, 0);
        }
        if (mapping != MAP_FAILED) munmap(mapping, mapping_size);
        for (int fd : sources) close(fd);
        close(epfd);
        if (mask_saved) sigprocmask(SIG_SETMASK, &original, nullptr);
        if (handler_saved) sigaction(SIGUSR1, &old_action, nullptr);
        alarm(0);
    }
    int ready(uint32_t flags = 0) {
        int fd = eventfd(1, EFD_NONBLOCK);
        if (fd < 0) return -1;
        sources.push_back(fd);
        epoll_event ev = {};
        ev.events = EPOLLIN | flags;
        ev.data.fd = fd;
        if (epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &ev) != 0) return -1;
        return fd;
    }
    void block_usr1() {
        sigset_t blocked;
        sigemptyset(&blocked);
        sigaddset(&blocked, SIGUSR1);
        ASSERT_EQ(0, sigprocmask(SIG_BLOCK, &blocked, &original));
        mask_saved = true;
        struct sigaction action = {};
        action.sa_handler = on_signal;
        action.sa_flags = SA_RESTART;
        sigemptyset(&action.sa_mask);
        ASSERT_EQ(0, sigaction(SIGUSR1, &action, &old_action));
        handler_saved = true;
        received = 0;
    }
    void expect_blocked() {
        sigset_t current;
        ASSERT_EQ(0, sigprocmask(SIG_SETMASK, nullptr, &current));
        EXPECT_EQ(1, sigismember(&current, SIGUSR1));
    }
    int epfd = -1;
    pid_t child = -1;
    std::vector<int> sources;
    timespec zero = {};
    epoll_event out[4] = {};
    sigset_t original = {};
    struct sigaction old_action = {};
    bool mask_saved = false, handler_saved = false;
    void* mapping = MAP_FAILED;
    size_t mapping_size = 0;
};

TEST_F(EpollPwait2, ZeroFiniteAndSubmillisecondTimeouts) {
    for (long ns : {0L, 1L, 500001L, 20000000L}) {
        timespec timeout = {0, ns}, before = {}, after = {};
        ASSERT_EQ(0, clock_gettime(CLOCK_MONOTONIC, &before));
        ASSERT_EQ(0, pwait2(epfd, out, 1, &timeout)) << errno;
        ASSERT_EQ(0, clock_gettime(CLOCK_MONOTONIC, &after));
        int64_t elapsed = (after.tv_sec - before.tv_sec) * INT64_C(1000000000) +
                          after.tv_nsec - before.tv_nsec;
        EXPECT_GE(elapsed, ns);
    }
}

TEST_F(EpollPwait2, NullAndHugeTimeoutWakeOnNewReadiness) {
    timespec huge = {INT64_MAX, 999999999};
    for (const timespec* timeout : {static_cast<const timespec*>(nullptr), static_cast<const timespec*>(&huge)}) {
        int fd = eventfd(0, 0);
        ASSERT_GE(fd, 0);
        sources.push_back(fd);
        epoll_event ev = {};
        ev.events = EPOLLIN;
        ev.data.fd = fd;
        ASSERT_EQ(0, epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &ev));
        child = fork();
        ASSERT_GE(child, 0);
        if (child == 0) {
            usleep(20000);
            uint64_t one = 1;
            _exit(write(fd, &one, sizeof(one)) == sizeof(one) ? 0 : 1);
        }
        ASSERT_EQ(1, pwait2(epfd, out, 1, timeout));
        EXPECT_EQ(fd, out[0].data.fd);
        int status;
        ASSERT_EQ(child, waitpid(child, &status, 0));
        child = -1;
        EXPECT_TRUE(WIFEXITED(status) && WEXITSTATUS(status) == 0);
        ASSERT_EQ(0, epoll_ctl(epfd, EPOLL_CTL_DEL, fd, nullptr));
    }
}

TEST_F(EpollPwait2, HugeTimeoutReturnsReadyEvent) {
    ASSERT_GE(ready(), 0);
    timespec huge = {INT64_MAX, 999999999};
    EXPECT_EQ(1, pwait2(epfd, out, 1, &huge));
}

TEST_F(EpollPwait2, ValidationOrderAndSixArgumentPwait) {
    const timespec* bad_time = reinterpret_cast<const timespec*>(1);
    const uint64_t* bad_mask = reinterpret_cast<const uint64_t*>(1);
    uint64_t mask = 0;
    for (timespec invalid : {timespec{-1, 0}, timespec{0, -1}, timespec{0, 1000000000}}) {
        EXPECT_EQ(-1, pwait2(-1, out, 0, &invalid, bad_mask, 1));
        EXPECT_EQ(EINVAL, errno);
    }
    EXPECT_EQ(-1, pwait2(-1, out, 0, bad_time, bad_mask, 1));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_EQ(-1, pwait2(-1, out, 1, &zero, bad_mask, 1));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(-1, pwait2(-1, out, 1, &zero, bad_mask));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_EQ(-1, pwait2(-1, out, 0, &zero));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(-1, pwait2(-1, out, 1, &zero));
    EXPECT_EQ(EBADF, errno);
    int ordinary = eventfd(0, 0);
    ASSERT_GE(ordinary, 0);
    sources.push_back(ordinary);
    EXPECT_EQ(-1, pwait2(ordinary, out, 1, &zero));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(-1, pwait2(epfd, out, INT_MAX, &zero));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(0, pwait2(epfd, out, 1, &zero, nullptr, 123));
    EXPECT_EQ(-1, pwait2(epfd, out, 1, &zero, &mask, sizeof(sigset_t)));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(-1, syscall(SYS_epoll_pwait, epfd, out, 1, 0, &mask, 1));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(0, syscall(SYS_epoll_pwait, epfd, out, 1, 0, &mask, kSigsetSize));
    EXPECT_EQ(0, syscall(SYS_epoll_pwait, epfd, out, 1, 0, nullptr, 123));
}

TEST_F(EpollPwait2, TemporaryMaskRestoredForSuccessTimeoutAndError) {
    block_usr1();
    uint64_t empty = 0;
    EXPECT_EQ(0, pwait2(epfd, out, 1, &zero, &empty));
    expect_blocked();
    EXPECT_EQ(-1, pwait2(-1, out, 1, &zero, &empty));
    EXPECT_EQ(EBADF, errno);
    expect_blocked();
    ASSERT_GE(ready(), 0);
    EXPECT_EQ(1, pwait2(epfd, out, 1, &zero, &empty));
    expect_blocked();
    EXPECT_EQ(1, syscall(SYS_epoll_pwait, epfd, out, 1, 0, &empty, kSigsetSize));
    expect_blocked();
    mapping_size = sysconf(_SC_PAGESIZE);
    mapping = mmap(nullptr, mapping_size, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping);
    EXPECT_EQ(-1, pwait2(epfd, static_cast<epoll_event*>(mapping), 1, &zero, &empty));
    EXPECT_EQ(EFAULT, errno);
    expect_blocked();
}

TEST_F(EpollPwait2, TemporarilyBlockedSignalIsDeliveredAfterFiniteWait) {
    block_usr1();
    sigset_t unblocked;
    sigemptyset(&unblocked);
    ASSERT_EQ(0, sigprocmask(SIG_SETMASK, &unblocked, nullptr));
    uint64_t mask = UINT64_C(1) << (SIGUSR1 - 1);
    for (bool legacy : {false, true}) {
        received = 0;
        child = fork();
        ASSERT_GE(child, 0);
        if (child == 0) {
            usleep(20000);
            _exit(kill(getppid(), SIGUSR1) == 0 ? 0 : 1);
        }
        timespec timeout = {0, 100000000}, before = {}, after = {};
        ASSERT_EQ(0, clock_gettime(CLOCK_MONOTONIC, &before));
        int result = legacy ? syscall(SYS_epoll_pwait, epfd, out, 1, 100, &mask, kSigsetSize)
                            : pwait2(epfd, out, 1, &timeout, &mask);
        EXPECT_EQ(0, result) << errno;
        ASSERT_EQ(0, clock_gettime(CLOCK_MONOTONIC, &after));
        EXPECT_GE((after.tv_sec - before.tv_sec) * INT64_C(1000000000) +
                      after.tv_nsec - before.tv_nsec, 100000000);
        EXPECT_EQ(1, received);
        sigset_t current;
        ASSERT_EQ(0, sigprocmask(SIG_SETMASK, nullptr, &current));
        EXPECT_EQ(0, sigismember(&current, SIGUSR1));
        int status;
        ASSERT_EQ(child, waitpid(child, &status, 0));
        child = -1;
        EXPECT_TRUE(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    }
}

TEST_F(EpollPwait2, TemporaryMaskCannotBlockStopOrKill) {
    child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        uint64_t mask = (UINT64_C(1) << (SIGSTOP - 1)) | (UINT64_C(1) << (SIGKILL - 1));
        // Keep waiting across SIGCONT even if the kernel returns EINTR.
        for (;;) {
            int result = pwait2(epfd, out, 1, nullptr, &mask);
            if (result != -1 || errno != EINTR) _exit(1);
        }
    }
    usleep(20000);
    ASSERT_EQ(0, kill(child, SIGSTOP));
    int status;
    ASSERT_EQ(child, waitpid(child, &status, WUNTRACED));
    ASSERT_TRUE(WIFSTOPPED(status));
    EXPECT_EQ(SIGSTOP, WSTOPSIG(status));
    ASSERT_EQ(0, kill(child, SIGCONT));
    usleep(20000);
    ASSERT_EQ(0, kill(child, SIGKILL));
    ASSERT_EQ(child, waitpid(child, &status, 0));
    child = -1;
    ASSERT_TRUE(WIFSIGNALED(status));
    EXPECT_EQ(SIGKILL, WTERMSIG(status));
}

TEST_F(EpollPwait2, PendingSignalPrecedesNonzeroTimeoutButNotZeroPoll) {
    block_usr1();
    uint64_t empty = 0;
    ASSERT_EQ(0, kill(getpid(), SIGUSR1));
    EXPECT_EQ(0, pwait2(epfd, out, 1, &zero, &empty));
    EXPECT_EQ(0, received);
    expect_blocked();
    sigset_t pending;
    ASSERT_EQ(0, sigpending(&pending));
    EXPECT_EQ(1, sigismember(&pending, SIGUSR1));
    for (long ns : {1L, 100L}) {
        if (ns == 100) {
            ASSERT_EQ(0, kill(getpid(), SIGUSR1));
        }
        received = 0;
        timespec timeout = {0, ns};
        EXPECT_EQ(-1, pwait2(epfd, out, 1, &timeout, &empty));
        EXPECT_EQ(EINTR, errno);
        EXPECT_EQ(1, received);
        expect_blocked();
        ASSERT_EQ(0, sigpending(&pending));
        EXPECT_EQ(0, sigismember(&pending, SIGUSR1));
    }
}

TEST_F(EpollPwait2, PendingSignalInterruptsWithRestartHandlerAndRestoresMask) {
    block_usr1();
    ASSERT_EQ(0, kill(getpid(), SIGUSR1));
    EXPECT_EQ(0, received);
    uint64_t empty = 0;
    timespec timeout = {1, 0};
    EXPECT_EQ(-1, pwait2(epfd, out, 1, &timeout, &empty));
    EXPECT_EQ(EINTR, errno);
    EXPECT_EQ(1, received);
    expect_blocked();
    received = 0;
    ASSERT_EQ(0, kill(getpid(), SIGUSR1));
    EXPECT_EQ(-1, syscall(SYS_epoll_pwait, epfd, out, 1, 1000, &empty, kSigsetSize));
    EXPECT_EQ(EINTR, errno);
    EXPECT_EQ(1, received);
    expect_blocked();
}

TEST_F(EpollPwait2, ReadOnlyOutputDoesNotConsumeEdgeOrOneshot) {
    mapping_size = sysconf(_SC_PAGESIZE);
    mapping = mmap(nullptr, mapping_size, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping);
    auto* readonly = static_cast<epoll_event*>(mapping);
    EXPECT_EQ(0, pwait2(epfd, readonly, 1, &zero));
    for (uint32_t flag : {uint32_t(EPOLLET), uint32_t(EPOLLONESHOT)}) {
        int fd = ready(flag);
        ASSERT_GE(fd, 0);
        EXPECT_EQ(-1, pwait2(epfd, readonly, 1, &zero));
        EXPECT_EQ(EFAULT, errno);
        EXPECT_EQ(1, pwait2(epfd, out, 1, &zero));
        EXPECT_EQ(fd, out[0].data.fd);
        EXPECT_EQ(0, pwait2(epfd, out, 1, &zero));
        ASSERT_EQ(0, epoll_ctl(epfd, EPOLL_CTL_DEL, fd, nullptr));
    }
}

TEST_F(EpollPwait2, CrossPagePartialSuccessPreservesUnwrittenEvent) {
    size_t page = sysconf(_SC_PAGESIZE);
    mapping_size = 2 * page;
    mapping = mmap(nullptr, mapping_size, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping);
    ASSERT_EQ(0, mprotect(static_cast<char*>(mapping) + page, page, PROT_NONE));
    ASSERT_GE(ready(EPOLLONESHOT), 0);
    ASSERT_GE(ready(EPOLLONESHOT), 0);
    // First event fits; the second event straddles the inaccessible page.
    auto* split = reinterpret_cast<epoll_event*>(static_cast<char*>(mapping) + page -
                                                sizeof(epoll_event) - 4);
    ASSERT_EQ(1, pwait2(epfd, split, 2, &zero));
    int delivered = split[0].data.fd;
    ASSERT_EQ(1, pwait2(epfd, out, 2, &zero));
    EXPECT_NE(delivered, out[0].data.fd);
    EXPECT_EQ(0, pwait2(epfd, out, 2, &zero));
}

TEST_F(EpollPwait2, MaxeventsDoesNotStarveLevelTriggeredSources) {
    for (int i = 0; i < 3; ++i) ASSERT_GE(ready(), 0);
    std::vector<int> seen;
    for (int i = 0; i < 3; ++i) {
        ASSERT_EQ(1, pwait2(epfd, out, 1, &zero));
        for (int fd : seen) EXPECT_NE(fd, out[0].data.fd);
        seen.push_back(out[0].data.fd);
    }
}

TEST_F(EpollPwait2, MaxeventsPreservesRemainingEdgesAndLegacyWait) {
    for (int i = 0; i < 3; ++i) ASSERT_GE(ready(EPOLLET), 0);
    std::vector<int> seen;
    for (int i = 0; i < 3; ++i) {
        ASSERT_EQ(1, epoll_wait(epfd, out, 1, 0));
        for (int fd : seen) EXPECT_NE(fd, out[0].data.fd);
        seen.push_back(out[0].data.fd);
    }
    EXPECT_EQ(0, pwait2(epfd, out, 1, &zero));
}
}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
