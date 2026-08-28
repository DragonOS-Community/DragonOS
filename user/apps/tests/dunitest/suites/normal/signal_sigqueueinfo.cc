#include <gtest/gtest.h>

#include <errno.h>
#include <signal.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#ifndef __NR_rt_sigqueueinfo
#error "__NR_rt_sigqueueinfo is required"
#endif

#ifndef SEGV_MAPERR
#define SEGV_MAPERR 1
#endif

#ifndef POLL_IN
#define POLL_IN 1
#endif

#ifndef SI_QUEUE
#define SI_QUEUE -1
#endif

namespace {

class ScopedSignalBlock {
public:
    explicit ScopedSignalBlock(int sig) : sig_(sig), active_(false) {
        sigset_t set;
        sigemptyset(&set);
        sigaddset(&set, sig_);
        if (sigprocmask(SIG_BLOCK, &set, &oldset_) == 0) {
            active_ = true;
        }
    }

    ~ScopedSignalBlock() {
        if (active_) {
            sigprocmask(SIG_SETMASK, &oldset_, nullptr);
        }
    }

    bool active() const {
        return active_;
    }

private:
    int sig_;
    bool active_;
    sigset_t oldset_ {};
};

void DrainPendingSignal(int sig) {
    sigset_t waitset;
    sigemptyset(&waitset);
    sigaddset(&waitset, sig);

    siginfo_t drained {};
    timespec zero {};
    while (sigtimedwait(&waitset, &drained, &zero) == sig) {
    }
}

void SendQueuedInfoToSelf(int sig, const siginfo_t& info) {
    errno = 0;
    long ret = syscall(__NR_rt_sigqueueinfo, getpid(), sig, &info);
    ASSERT_EQ(0, ret) << "rt_sigqueueinfo failed: errno=" << errno << " ("
                      << strerror(errno) << ")";
}

siginfo_t WaitForSignalInfo(int sig) {
    sigset_t waitset;
    sigemptyset(&waitset);
    sigaddset(&waitset, sig);

    siginfo_t received {};
    timespec timeout {};
    timeout.tv_sec = 2;
    int ret = sigtimedwait(&waitset, &received, &timeout);
    EXPECT_EQ(sig, ret) << "sigtimedwait failed: errno=" << errno << " ("
                        << strerror(errno) << ")";
    return received;
}

}  // namespace

TEST(SignalSigqueueinfo, SegvMaperrPreservesFaultAddress) {
    ScopedSignalBlock block(SIGSEGV);
    ASSERT_TRUE(block.active()) << "sigprocmask(SIG_BLOCK, SIGSEGV) failed";
    DrainPendingSignal(SIGSEGV);

    constexpr uintptr_t kFaultAddress = 0x12345000;
    siginfo_t info {};
    info.si_signo = SIGSEGV;
    info.si_errno = 0;
    info.si_code = SEGV_MAPERR;
    info.si_addr = reinterpret_cast<void*>(kFaultAddress);

    SendQueuedInfoToSelf(SIGSEGV, info);
    siginfo_t received = WaitForSignalInfo(SIGSEGV);

    EXPECT_EQ(SIGSEGV, received.si_signo);
    EXPECT_EQ(SEGV_MAPERR, received.si_code);
    EXPECT_EQ(reinterpret_cast<void*>(kFaultAddress), received.si_addr);
}

TEST(SignalSigqueueinfo, PositivePollCodePreservesPollFields) {
    ScopedSignalBlock block(SIGUSR1);
    ASSERT_TRUE(block.active()) << "sigprocmask(SIG_BLOCK, SIGUSR1) failed";
    DrainPendingSignal(SIGUSR1);

    constexpr long kBand = 0x41;
    constexpr int kFd = 123;
    siginfo_t info {};
    info.si_signo = SIGUSR1;
    info.si_errno = 0;
    info.si_code = POLL_IN;
    info.si_band = kBand;
    info.si_fd = kFd;

    SendQueuedInfoToSelf(SIGUSR1, info);
    siginfo_t received = WaitForSignalInfo(SIGUSR1);

    EXPECT_EQ(SIGUSR1, received.si_signo);
    EXPECT_EQ(POLL_IN, received.si_code);
    EXPECT_EQ(kBand, received.si_band);
    EXPECT_EQ(kFd, received.si_fd);
}

TEST(SignalSigqueueinfo, SiQueuePreservesRtFields) {
    ScopedSignalBlock block(SIGUSR2);
    ASSERT_TRUE(block.active()) << "sigprocmask(SIG_BLOCK, SIGUSR2) failed";
    DrainPendingSignal(SIGUSR2);

    constexpr int kPid = 1234;
    constexpr int kUid = 5678;
    constexpr int kValue = 0x1357;
    siginfo_t info {};
    info.si_signo = SIGUSR2;
    info.si_errno = 0;
    info.si_code = SI_QUEUE;
    info.si_pid = kPid;
    info.si_uid = kUid;
    info.si_value.sival_int = kValue;

    SendQueuedInfoToSelf(SIGUSR2, info);
    siginfo_t received = WaitForSignalInfo(SIGUSR2);

    EXPECT_EQ(SIGUSR2, received.si_signo);
    EXPECT_EQ(SI_QUEUE, received.si_code);
    EXPECT_EQ(kPid, received.si_pid);
    EXPECT_EQ(static_cast<uid_t>(kUid), received.si_uid);
    EXPECT_EQ(kValue, received.si_value.sival_int);
}

TEST(SignalSigqueueinfo, KnownLayoutOnlyRequiresKernelSiginfoBytesReadable) {
    ScopedSignalBlock block(SIGUSR2);
    ASSERT_TRUE(block.active());
    DrainPendingSignal(SIGUSR2);

    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    void* mapping = mmap(nullptr, static_cast<size_t>(page_size) * 2,
                         PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1,
                         0);
    ASSERT_NE(MAP_FAILED, mapping);
    auto* info = reinterpret_cast<siginfo_t*>(
        static_cast<unsigned char*>(mapping) + page_size - 48);
    siginfo_t source {};
    source.si_signo = SIGUSR2;
    source.si_code = SI_QUEUE;
    source.si_pid = getpid();
    source.si_uid = getuid();
    source.si_value.sival_int = 0x2468;
    memcpy(info, &source, 48);
    ASSERT_EQ(0, mprotect(static_cast<unsigned char*>(mapping) + page_size,
                          page_size, PROT_NONE));

    errno = 0;
    ASSERT_EQ(0, syscall(__NR_rt_sigqueueinfo, getpid(), SIGUSR2, info))
        << strerror(errno);
    siginfo_t received = WaitForSignalInfo(SIGUSR2);
    EXPECT_EQ(0x2468, received.si_value.sival_int);
    EXPECT_EQ(0, munmap(mapping, static_cast<size_t>(page_size) * 2));
}

TEST(SignalSigqueueinfo, ValidationUsesSyscallSignalAndStillRunsForSignalZero) {
    ScopedSignalBlock block(SIGUSR1);
    ASSERT_TRUE(block.active());
    DrainPendingSignal(SIGUSR1);

    siginfo_t info {};
    info.si_signo = SIGSYS;
    info.si_code = 3;
    info.si_band = 0x3141;
    info.si_fd = 27;
    reinterpret_cast<unsigned char*>(&info)[48] = 1;
    ASSERT_EQ(0, syscall(__NR_rt_sigqueueinfo, getpid(), SIGUSR1, &info));
    siginfo_t received = WaitForSignalInfo(SIGUSR1);
    EXPECT_EQ(SIGUSR1, received.si_signo);
    EXPECT_EQ(3, received.si_code);
    EXPECT_EQ(0x3141, received.si_band);
    EXPECT_EQ(27, received.si_fd);

    memset(&info, 0, sizeof(info));
    info.si_code = 7;
    reinterpret_cast<unsigned char*>(&info)[48] = 1;
    errno = 0;
    EXPECT_EQ(-1, syscall(__NR_rt_sigqueueinfo, getpid(), 0, &info));
    EXPECT_EQ(E2BIG, errno);
}

#if defined(__NR_pidfd_open) && defined(__NR_pidfd_send_signal)
TEST(SignalSigqueueinfo, PidfdSignalZeroStillValidatesSiginfo) {
    const int pidfd = static_cast<int>(syscall(__NR_pidfd_open, getpid(), 0));
    ASSERT_GE(pidfd, 0) << strerror(errno);

    siginfo_t info {};
    info.si_signo = SIGUSR1;
    errno = 0;
    EXPECT_EQ(-1, syscall(__NR_pidfd_send_signal, pidfd, 0, &info, 0));
    EXPECT_EQ(EINVAL, errno);

    errno = 0;
    EXPECT_EQ(-1, syscall(__NR_pidfd_send_signal, pidfd, 0,
                          reinterpret_cast<void*>(1), 0));
    EXPECT_EQ(EFAULT, errno);

    memset(&info, 0, sizeof(info));
    info.si_code = 7;
    reinterpret_cast<unsigned char*>(&info)[48] = 1;
    errno = 0;
    EXPECT_EQ(-1, syscall(__NR_pidfd_send_signal, pidfd, 0, &info, 0));
    EXPECT_EQ(E2BIG, errno);
    close(pidfd);
}
#endif

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
