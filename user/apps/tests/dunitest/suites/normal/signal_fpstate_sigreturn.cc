#include <gtest/gtest.h>

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/ucontext.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef SS_AUTODISARM
#define SS_AUTODISARM (1U << 31)
#endif

namespace {

constexpr size_t kAltStackSize = 64 * 1024;
alignas(16) uint8_t g_alt_stack[kAltStackSize];
volatile uintptr_t g_handler_sp = 0;
volatile int g_handler_ss_flags = -1;
volatile int g_handler_sigaltstack_errno = 0;

bool ptr_in_alt_stack(uintptr_t ptr) {
    uintptr_t begin = reinterpret_cast<uintptr_t>(g_alt_stack);
    uintptr_t end = begin + kAltStackSize;
    return ptr >= begin && ptr < end;
}

void corrupt_fpstate_handler(int, siginfo_t*, void* raw_ucontext) {
#if defined(__x86_64__)
    auto* ctx = reinterpret_cast<ucontext_t*>(raw_ucontext);
    if (ctx == nullptr || ctx->uc_mcontext.fpregs == nullptr) {
        _exit(2);
    }

    auto* fpstate = reinterpret_cast<volatile uint8_t*>(ctx->uc_mcontext.fpregs);
    for (size_t i = 0; i < 32; ++i) {
        fpstate[i] = 0xff;
    }
#else
    (void)raw_ucontext;
    _exit(3);
#endif
}

void run_corrupt_sigreturn_child() {
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_sigaction = corrupt_fpstate_handler;
    sigemptyset(&action.sa_mask);
    action.sa_flags = SA_SIGINFO;

    if (sigaction(SIGUSR1, &action, nullptr) != 0) {
        _exit(4);
    }

    if (raise(SIGUSR1) != 0) {
        _exit(5);
    }

    _exit(6);
}

void autodisarm_handler(int) {
    uint8_t marker = 0;
    g_handler_sp = reinterpret_cast<uintptr_t>(&marker);

    stack_t current {};
    if (sigaltstack(nullptr, &current) != 0) {
        g_handler_sigaltstack_errno = errno;
        _exit(20);
    }
    g_handler_ss_flags = current.ss_flags;
}

void run_autodisarm_child() {
    stack_t ss {};
    ss.ss_sp = g_alt_stack;
    ss.ss_size = sizeof(g_alt_stack);
    ss.ss_flags = SS_AUTODISARM;
    if (sigaltstack(&ss, nullptr) != 0) {
        _exit(21);
    }

    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = autodisarm_handler;
    sigemptyset(&action.sa_mask);
    action.sa_flags = SA_ONSTACK;
    if (sigaction(SIGUSR1, &action, nullptr) != 0) {
        _exit(22);
    }

    if (raise(SIGUSR1) != 0) {
        _exit(23);
    }

    if (!ptr_in_alt_stack(g_handler_sp)) {
        _exit(24);
    }
    if (g_handler_sigaltstack_errno != 0) {
        _exit(25);
    }
    if ((g_handler_ss_flags & SS_DISABLE) == 0) {
        _exit(26);
    }

    stack_t restored {};
    if (sigaltstack(nullptr, &restored) != 0) {
        _exit(27);
    }
    if (restored.ss_sp != g_alt_stack || restored.ss_size != sizeof(g_alt_stack)) {
        _exit(28);
    }
    if ((restored.ss_flags & SS_AUTODISARM) == 0 || (restored.ss_flags & SS_DISABLE) != 0) {
        _exit(29);
    }

    stack_t disable {};
    disable.ss_flags = SS_DISABLE;
    sigaltstack(&disable, nullptr);
    _exit(0);
}

void invalid_uc_stack_handler(int, siginfo_t*, void* raw_ucontext) {
    auto* ctx = reinterpret_cast<ucontext_t*>(raw_ucontext);
    if (ctx == nullptr) {
        _exit(30);
    }
    ctx->uc_stack.ss_flags = 0x400;
}

void run_invalid_uc_stack_child() {
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_sigaction = invalid_uc_stack_handler;
    sigemptyset(&action.sa_mask);
    action.sa_flags = SA_SIGINFO;
    if (sigaction(SIGUSR2, &action, nullptr) != 0) {
        _exit(31);
    }

    if (raise(SIGUSR2) != 0) {
        _exit(32);
    }

    _exit(0);
}

}  // namespace

TEST(SignalFpstateSigreturn, InvalidMxcsrKillsProcessWithoutKernelFault) {
    pid_t child = fork();
    ASSERT_GE(child, 0);

    if (child == 0) {
        run_corrupt_sigreturn_child();
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFSIGNALED(status)) << "child exited with status " << WEXITSTATUS(status);
    EXPECT_EQ(SIGSEGV, WTERMSIG(status));
}

TEST(SignalFpstateSigreturn, SigaltstackAcceptsOnstackInstallFlag) {
    stack_t ss {};
    ss.ss_sp = g_alt_stack;
    ss.ss_size = sizeof(g_alt_stack);
    ss.ss_flags = SS_ONSTACK;
    ASSERT_EQ(0, sigaltstack(&ss, nullptr)) << strerror(errno);

    stack_t disable {};
    disable.ss_flags = SS_DISABLE;
    ASSERT_EQ(0, sigaltstack(&disable, nullptr)) << strerror(errno);

    ss.ss_flags = SS_ONSTACK | SS_AUTODISARM;
    ASSERT_EQ(0, sigaltstack(&ss, nullptr)) << strerror(errno);
    ASSERT_EQ(0, sigaltstack(&disable, nullptr)) << strerror(errno);
}

TEST(SignalFpstateSigreturn, AutodisarmResetsDuringHandlerAndRestoresOnSigreturn) {
    pid_t child = fork();
    ASSERT_GE(child, 0);

    if (child == 0) {
        run_autodisarm_child();
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status)) << "child killed by signal " << WTERMSIG(status);
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(SignalFpstateSigreturn, InvalidUcStackRestoreErrorIsIgnored) {
    pid_t child = fork();
    ASSERT_GE(child, 0);

    if (child == 0) {
        run_invalid_uc_stack_child();
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status)) << "child killed by signal " << WTERMSIG(status);
    EXPECT_EQ(0, WEXITSTATUS(status));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
