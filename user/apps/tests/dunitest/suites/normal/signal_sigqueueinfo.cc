#include <gtest/gtest.h>

#include <errno.h>
#include <signal.h>
#include <string.h>
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

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
