#include <gtest/gtest.h>

#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>
#include <vector>

#ifndef SYS_ioperm
#define SYS_ioperm 173
#endif

#ifndef CLONE_NEWUSER
#define CLONE_NEWUSER 0x10000000
#endif

namespace {

constexpr uint16_t kPort = 0x80;
constexpr uint16_t kSecondPort = 0x81;
constexpr size_t kCloneStackSize = 1 << 20;

const char* g_program_path = nullptr;

int ioperm_errno(unsigned long from, unsigned long num, int turn_on) {
    errno = 0;
    long ret = syscall(SYS_ioperm, from, num, turn_on);
    if (ret == 0) {
        return 0;
    }
    return errno;
}

void outb(uint16_t port) {
#if defined(__x86_64__)
    uint8_t value = 0;
    __asm__ __volatile__("outb %0, %w1" : : "a"(value), "Nd"(port) : "memory");
#endif
}

int run_outb_child(uint16_t port) {
    pid_t child = fork();
    if (child < 0) {
        return -errno;
    }

    if (child == 0) {
        outb(port);
        _exit(0);
    }

    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        return -errno;
    }
    return status;
}

void expect_outb_sigsegv(uint16_t port) {
    int status = run_outb_child(port);
    ASSERT_GE(status, 0);
    ASSERT_TRUE(WIFSIGNALED(status)) << "status=" << status;
    EXPECT_EQ(SIGSEGV, WTERMSIG(status));
}

int wait_for_child(pid_t child) {
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        return -errno;
    }
    return status;
}

int new_userns_child(void*) {
    int grant_err = ioperm_errno(kPort, 1, 1);
    if (grant_err != EPERM) {
        return grant_err == 0 ? 10 : grant_err;
    }
    return ioperm_errno(kPort, 1, 0);
}

int run_in_new_userns() {
    std::vector<char> stack(kCloneStackSize);
    pid_t child = clone(new_userns_child, stack.data() + stack.size(), CLONE_NEWUSER | SIGCHLD,
                        nullptr);
    if (child < 0) {
        return -errno;
    }
    return wait_for_child(child);
}

int exec_child_main() {
    outb(kPort);
    return 0;
}

class IopermSemantics : public ::testing::Test {
protected:
    void SetUp() override {
        (void)ioperm_errno(kPort, 1, 0);
        (void)ioperm_errno(kSecondPort, 1, 0);
    }

    void TearDown() override {
        (void)ioperm_errno(kPort, 1, 0);
        (void)ioperm_errno(kSecondPort, 1, 0);
    }
};

}  // namespace

TEST_F(IopermSemantics, DefaultOutbWithoutIopermGetsSigsegv) {
    expect_outb_sigsegv(kPort);
}

TEST_F(IopermSemantics, InvalidRangesReturnEinval) {
    EXPECT_EQ(EINVAL, ioperm_errno(kPort, 0, 1));
    EXPECT_EQ(EINVAL, ioperm_errno(65535, 2, 1));
    EXPECT_EQ(EINVAL, ioperm_errno(~0UL, 2, 1));
}

TEST_F(IopermSemantics, RevokeWithoutBitmapSucceeds) {
    EXPECT_EQ(0, ioperm_errno(kPort, 1, 0));
}

TEST_F(IopermSemantics, GrantAllowsImmediateOutbAndRevokeDenies) {
    ASSERT_EQ(0, ioperm_errno(kPort, 1, 1));
    outb(kPort);

    ASSERT_EQ(0, ioperm_errno(kPort, 1, 0));
    expect_outb_sigsegv(kPort);
}

TEST_F(IopermSemantics, ForkInheritsPermissionAndMutationIsCopyOnWrite) {
    ASSERT_EQ(0, ioperm_errno(kPort, 1, 1));

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        int status = run_outb_child(kPort);
        if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
            _exit(11);
        }

        if (ioperm_errno(kPort, 1, 0) != 0) {
            _exit(12);
        }

        status = run_outb_child(kPort);
        if (!WIFSIGNALED(status) || WTERMSIG(status) != SIGSEGV) {
            _exit(13);
        }
        _exit(0);
    }

    int status = wait_for_child(child);
    ASSERT_GE(status, 0);
    ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
    ASSERT_EQ(0, WEXITSTATUS(status));

    outb(kPort);
}

TEST_F(IopermSemantics, ExecPreservesPermission) {
    ASSERT_NE(nullptr, g_program_path);

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (ioperm_errno(kPort, 1, 1) != 0) {
            _exit(11);
        }
        setenv("DRAGONOS_IOPERM_EXEC_CHILD", "1", 1);
        execl(g_program_path, g_program_path, nullptr);
        _exit(errno == 0 ? 12 : errno);
    }

    int status = wait_for_child(child);
    ASSERT_GE(status, 0);
    ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST_F(IopermSemantics, GrantRequiresCapSysRawioButRevokeDoesNot) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (setresuid(1000, 1000, 1000) != 0) {
            _exit(errno == 0 ? 11 : errno);
        }

        int grant_err = ioperm_errno(kPort, 1, 1);
        if (grant_err != EPERM) {
            _exit(grant_err == 0 ? 12 : grant_err);
        }

        int revoke_err = ioperm_errno(kPort, 1, 0);
        _exit(revoke_err);
    }

    int status = wait_for_child(child);
    ASSERT_GE(status, 0);
    ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST_F(IopermSemantics, NewUserNamespaceRootStillCannotGrantRawIo) {
    int status = run_in_new_userns();
    if (status == -ENOSYS || status == -EINVAL || status == -EPERM) {
        GTEST_SKIP() << "CLONE_NEWUSER unavailable: " << strerror(-status);
    }

    ASSERT_GE(status, 0);
    ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status));
}

int main(int argc, char** argv) {
    if (getenv("DRAGONOS_IOPERM_EXEC_CHILD") != nullptr) {
        return exec_child_main();
    }

    g_program_path = argv[0];
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
