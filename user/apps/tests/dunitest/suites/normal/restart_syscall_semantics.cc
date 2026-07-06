#include <gtest/gtest.h>

#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_restart_syscall
#if defined(__x86_64__)
#define SYS_restart_syscall 219
#elif defined(__riscv) || defined(__loongarch64)
#define SYS_restart_syscall 128
#else
#error "SYS_restart_syscall is not defined for this architecture"
#endif
#endif

#ifndef SYS_clock_nanosleep
#if defined(__x86_64__)
#define SYS_clock_nanosleep 230
#elif defined(__riscv) || defined(__loongarch64)
#define SYS_clock_nanosleep 115
#else
#error "SYS_clock_nanosleep is not defined for this architecture"
#endif
#endif

namespace {

volatile sig_atomic_t g_signal_count = 0;

void SignalHandler(int) {
    ++g_signal_count;
}

struct SignalAfterArgs {
    pthread_t target;
    int signo;
    int send_result;
};

void* SendSignalAfterDelay(void* raw_args) {
    auto* args = static_cast<SignalAfterArgs*>(raw_args);
    timespec delay {};
    delay.tv_nsec = 100 * 1000 * 1000;
    nanosleep(&delay, nullptr);
    args->send_result = pthread_kill(args->target, args->signo);
    return nullptr;
}

long RawClockNanosleep(clockid_t clockid, int flags, const timespec* request,
                       timespec* remain) {
    return syscall(SYS_clock_nanosleep, static_cast<int>(clockid), flags, request,
                   remain);
}

}  // namespace

TEST(RestartSyscallSemantics, DirectCallWithoutRestartBlockReturnsEintr) {
    errno = 0;
    long ret = syscall(SYS_restart_syscall);

    EXPECT_EQ(-1, ret);
    EXPECT_EQ(EINTR, errno) << "restart_syscall returned ret=" << ret
                            << ", errno=" << errno << " (" << strerror(errno)
                            << ")";
}

TEST(RestartSyscallSemantics, RestartBlockDeliveredHandlerReturnsEintrEvenWithSaRestart) {
    struct sigaction action {};
    struct sigaction old_action {};
    action.sa_handler = SignalHandler;
    sigemptyset(&action.sa_mask);
    action.sa_flags = SA_RESTART;
    ASSERT_EQ(0, sigaction(SIGUSR1, &action, &old_action))
        << "sigaction failed: errno=" << errno << " (" << strerror(errno) << ")";

    g_signal_count = 0;
    SignalAfterArgs args {};
    args.target = pthread_self();
    args.signo = SIGUSR1;
    args.send_result = -1;

    pthread_t sender;
    ASSERT_EQ(0, pthread_create(&sender, nullptr, SendSignalAfterDelay, &args));

    errno = 0;
    timespec request {};
    request.tv_sec = 2;
    timespec remain {};
    long ret = RawClockNanosleep(CLOCK_REALTIME, 0, &request, &remain);
    int saved_errno = errno;

    ASSERT_EQ(0, pthread_join(sender, nullptr));

    EXPECT_EQ(0, args.send_result);
    EXPECT_EQ(1, g_signal_count);
    EXPECT_EQ(-1, ret);
    EXPECT_EQ(EINTR, saved_errno)
        << "clock_nanosleep returned ret=" << ret << ", errno=" << saved_errno
        << " (" << strerror(saved_errno) << "), remain={" << remain.tv_sec << ", "
        << remain.tv_nsec << "}";

    errno = 0;
    long restart_ret = syscall(SYS_restart_syscall);
    int restart_errno = errno;
    EXPECT_EQ(-1, restart_ret);
    EXPECT_EQ(EINTR, restart_errno)
        << "stale restart block was consumed after signal handler returned";

    EXPECT_EQ(0, sigaction(SIGUSR1, &old_action, nullptr));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
