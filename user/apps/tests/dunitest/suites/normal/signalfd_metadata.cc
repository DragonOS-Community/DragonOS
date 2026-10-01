#include <gtest/gtest.h>

#include <cerrno>
#include <cstdint>
#include <poll.h>
#include <signal.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

namespace {

class SignalFdMetadata : public testing::Test {
  protected:
    void SetUp() override {
        sigemptyset(&mask_);
        sigaddset(&mask_, SIGUSR1);
        sigaddset(&mask_, SIGCHLD);
        ASSERT_EQ(0, sigprocmask(SIG_BLOCK, &mask_, &old_mask_));
        blocked_ = true;
        fd_ = signalfd(-1, &mask_, SFD_NONBLOCK | SFD_CLOEXEC);
        ASSERT_GE(fd_, 0);
    }

    void TearDown() override {
        bool safe_to_unblock = true;
        if (timer_ >= 0) {
            const long result = syscall(SYS_timer_delete, timer_);
            EXPECT_EQ(0, result);
            safe_to_unblock = result == 0;
        }
        if (child_ > 0) {
            int status;
            EXPECT_EQ(child_, waitpid(child_, &status, 0));
        }
        if (fd_ >= 0) {
            signalfd_siginfo info{};
            for (int attempt = 0; attempt < 8; ++attempt) {
                if (read(fd_, &info, sizeof(info)) != sizeof(info)) {
                    break;
                }
            }
            EXPECT_EQ(0, close(fd_));
        }
        if (blocked_ && safe_to_unblock) {
            EXPECT_EQ(0, sigprocmask(SIG_SETMASK, &old_mask_, nullptr));
        }
    }

    bool Receive(signalfd_siginfo* info) {
        pollfd pfd{fd_, POLLIN, 0};
        if (poll(&pfd, 1, 2000) != 1 || !(pfd.revents & POLLIN)) {
            return false;
        }
        return read(fd_, info, sizeof(*info)) == sizeof(*info);
    }

    void ExpectHeader(const signalfd_siginfo& info, int signo, int code) {
        EXPECT_EQ(static_cast<uint32_t>(signo), info.ssi_signo);
        EXPECT_EQ(0, info.ssi_errno);
        EXPECT_EQ(code, info.ssi_code);
        for (unsigned char byte : info.__pad) {
            EXPECT_EQ(0, byte);
        }
    }

    sigset_t mask_{};
    sigset_t old_mask_{};
    bool blocked_ = false;
    int fd_ = -1;
    int timer_ = -1;
    pid_t child_ = -1;
};

TEST_F(SignalFdMetadata, UserSignalRetainsSender) {
    ASSERT_EQ(0, kill(getpid(), SIGUSR1));
    signalfd_siginfo info{};
    ASSERT_TRUE(Receive(&info));
    ExpectHeader(info, SIGUSR1, SI_USER);
    EXPECT_EQ(static_cast<uint32_t>(getpid()), info.ssi_pid);
    EXPECT_EQ(static_cast<uint32_t>(getuid()), info.ssi_uid);
    EXPECT_EQ(0u, info.ssi_ptr);
}

TEST_F(SignalFdMetadata, QueuedSignalRetainsFullSigval) {
    constexpr uintptr_t pointer = 0x1122334455667788ULL;
    union sigval value{};
    value.sival_ptr = reinterpret_cast<void*>(pointer);
    ASSERT_EQ(0, sigqueue(getpid(), SIGUSR1, value));
    signalfd_siginfo info{};
    ASSERT_TRUE(Receive(&info));
    ExpectHeader(info, SIGUSR1, SI_QUEUE);
    EXPECT_EQ(static_cast<uint32_t>(getpid()), info.ssi_pid);
    EXPECT_EQ(static_cast<uint32_t>(getuid()), info.ssi_uid);
    EXPECT_EQ(pointer, info.ssi_ptr);
    EXPECT_EQ(static_cast<int32_t>(pointer), info.ssi_int);
}

TEST_F(SignalFdMetadata, ChildExitRetainsStatus) {
    child_ = fork();
    ASSERT_GE(child_, 0);
    if (child_ == 0) {
        _exit(37);
    }
    signalfd_siginfo info{};
    ASSERT_TRUE(Receive(&info));
    ExpectHeader(info, SIGCHLD, CLD_EXITED);
    EXPECT_EQ(static_cast<uint32_t>(child_), info.ssi_pid);
    EXPECT_EQ(static_cast<uint32_t>(getuid()), info.ssi_uid);
    EXPECT_EQ(37, info.ssi_status);
    int status;
    ASSERT_EQ(child_, waitpid(child_, &status, 0));
    child_ = -1;
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(37, WEXITSTATUS(status));
}

TEST_F(SignalFdMetadata, PosixTimerRetainsTimerIdAndValue) {
    constexpr uintptr_t pointer = 0x1234567876543210ULL;
    sigevent event{};
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGUSR1;
    event.sigev_value.sival_ptr = reinterpret_cast<void*>(pointer);
    ASSERT_EQ(0, syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &timer_));
    itimerspec config{};
    config.it_value.tv_nsec = 20000000;
    ASSERT_EQ(0, syscall(SYS_timer_settime, timer_, 0, &config, nullptr));
    signalfd_siginfo info{};
    ASSERT_TRUE(Receive(&info));
    ExpectHeader(info, SIGUSR1, SI_TIMER);
    EXPECT_EQ(static_cast<uint32_t>(timer_), info.ssi_tid);
    EXPECT_EQ(0u, info.ssi_overrun);
    EXPECT_EQ(pointer, info.ssi_ptr);
    EXPECT_EQ(static_cast<int32_t>(pointer), info.ssi_int);
}

} // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
