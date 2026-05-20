#include <gtest/gtest.h>

#include <signal.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

void trigger_user_general_protection_fault() {
#if defined(__x86_64__)
    __asm__ __volatile__("int $13");
#endif
    _exit(42);
}

}  // namespace

TEST(GeneralProtectionSignal, UserFaultKillsChildWithSigsegv) {
    pid_t child = fork();
    ASSERT_GE(child, 0);

    if (child == 0) {
        trigger_user_general_protection_fault();
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
