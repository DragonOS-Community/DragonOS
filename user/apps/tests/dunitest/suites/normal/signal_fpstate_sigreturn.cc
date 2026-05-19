#include <gtest/gtest.h>

#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/ucontext.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

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

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
