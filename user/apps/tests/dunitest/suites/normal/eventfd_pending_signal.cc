#include <gtest/gtest.h>

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

namespace {

class UniqueFd {
public:
    UniqueFd() = default;
    explicit UniqueFd(int fd) : fd_(fd) {}
    UniqueFd(const UniqueFd&) = delete;
    UniqueFd& operator=(const UniqueFd&) = delete;

    ~UniqueFd() { reset(); }

    int get() const { return fd_; }

    void reset(int fd = -1) {
        if (fd_ >= 0) {
            close(fd_);
        }
        fd_ = fd;
    }

private:
    int fd_ = -1;
};

void BlockSignal(int signum, sigset_t* old_mask) {
    sigset_t blocked;
    sigemptyset(&blocked);
    sigaddset(&blocked, signum);
    ASSERT_EQ(0, sigprocmask(SIG_BLOCK, &blocked, old_mask)) << strerror(errno);
}

void RestoreSigmask(const sigset_t& old_mask) {
    ASSERT_EQ(0, sigprocmask(SIG_SETMASK, &old_mask, nullptr)) << strerror(errno);
}

void SendQueuedSignalToSelf(int signum) {
    siginfo_t info {};
    info.si_signo = signum;
    info.si_errno = 0;
    info.si_code = SI_QUEUE;
    info.si_pid = getpid();
    info.si_uid = getuid();
    info.si_value.sival_int = 0xefd;

    errno = 0;
    long ret = syscall(__NR_rt_sigqueueinfo, getpid(), signum, &info);
    ASSERT_EQ(0, ret) << "rt_sigqueueinfo failed: errno=" << errno << " (" << strerror(errno)
                      << ")";
}

void ExpectQueuedSignal(int signum) {
    sigset_t waitset;
    sigemptyset(&waitset);
    sigaddset(&waitset, signum);

    siginfo_t received {};
    timespec timeout {};
    timeout.tv_sec = 2;
    int ret = sigtimedwait(&waitset, &received, &timeout);
    ASSERT_EQ(signum, ret) << "sigtimedwait failed: errno=" << errno << " (" << strerror(errno)
                           << ")";
    EXPECT_EQ(SI_QUEUE, received.si_code);
    EXPECT_EQ(0xefd, received.si_value.sival_int);
}

TEST(EventFdPendingSignal, WriteSucceedsWhenCounterHasSpace) {
    sigset_t old_mask;
    BlockSignal(SIGUSR1, &old_mask);
    SendQueuedSignalToSelf(SIGUSR1);

    UniqueFd fd(eventfd(0, EFD_NONBLOCK));
    ASSERT_GE(fd.get(), 0) << "eventfd failed: " << strerror(errno);

    uint64_t value = 1;
    ssize_t written = write(fd.get(), &value, sizeof(value));
    EXPECT_EQ(static_cast<ssize_t>(sizeof(value)), written)
        << "eventfd write should not fail merely because a signal is pending, errno=" << errno
        << " (" << strerror(errno) << ")";

    uint64_t observed = 0;
    ssize_t read_bytes = read(fd.get(), &observed, sizeof(observed));
    ASSERT_EQ(static_cast<ssize_t>(sizeof(observed)), read_bytes)
        << "eventfd read failed after successful write, errno=" << errno << " (" << strerror(errno)
        << ")";
    EXPECT_EQ(value, observed);

    ExpectQueuedSignal(SIGUSR1);
    RestoreSigmask(old_mask);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
