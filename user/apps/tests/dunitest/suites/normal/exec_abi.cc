#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sched.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include <vector>

namespace {

char g_self_path[PATH_MAX] = {};

int check_auxv_credentials() {
    if (getauxval(AT_UID) != static_cast<unsigned long>(getuid())) {
        return 41;
    }
    if (getauxval(AT_EUID) != static_cast<unsigned long>(geteuid())) {
        return 42;
    }
    if (getauxval(AT_GID) != static_cast<unsigned long>(getgid())) {
        return 43;
    }
    if (getauxval(AT_EGID) != static_cast<unsigned long>(getegid())) {
        return 44;
    }
    return 0;
}

#if defined(__x86_64__)

constexpr unsigned char kCheckRdxElf[] = {
    0x7f, 0x45, 0x4c, 0x46, 0x02, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x3e, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x78, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x38, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00,

    0x01, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x92, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x92, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,

    // _start:
    //   test %rdx,%rdx
    //   jnz fail
    //   exit(0)
    // fail:
    //   exit(42)
    0x48, 0x85, 0xd2, 0x75, 0x09, 0x31, 0xff, 0xb8, 0x3c, 0x00, 0x00, 0x00,
    0x0f, 0x05, 0xbf, 0x2a, 0x00, 0x00, 0x00, 0xb8, 0x3c, 0x00, 0x00, 0x00,
    0x0f, 0x05,
};

constexpr unsigned char kCheckRobustListClearedElf[] = {
    0x7f, 0x45, 0x4c, 0x46, 0x02, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x3e, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x78, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x38, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00,

    0x01, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0xb6, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0xb6, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,

    // get_robust_list(0, rsp, rsp + 8); require a NULL head and sizeof(head).
    0x48, 0x83, 0xec, 0x20, 0x31, 0xff, 0x48, 0x8d, 0x34, 0x24, 0x48, 0x8d,
    0x54, 0x24, 0x08, 0xb8, 0x12, 0x01, 0x00, 0x00, 0x0f, 0x05, 0x85, 0xc0,
    0x75, 0x18, 0x48, 0x83, 0x3c, 0x24, 0x00, 0x75, 0x11, 0x48, 0x83, 0x7c,
    0x24, 0x08, 0x18, 0x75, 0x09, 0x31, 0xff, 0xb8, 0x3c, 0x00, 0x00, 0x00,
    0x0f, 0x05, 0xbf, 0x2a, 0x00, 0x00, 0x00, 0xb8, 0x3c, 0x00, 0x00, 0x00,
    0x0f, 0x05,
};

struct TestRobustListHead {
    uintptr_t next;
    intptr_t futex_offset;
    uintptr_t list_op_pending;
};

struct TestRobustNode {
    uintptr_t next;
    uint32_t futex;
};

constexpr uint32_t kFutexTidMask = 0x3fffffffU;
constexpr uint32_t kFutexOwnerDied = 0x40000000U;

void write_all(int fd, const void* data, size_t size) {
    const char* p = static_cast<const char*>(data);
    while (size > 0) {
        ssize_t n = write(fd, p, size);
        ASSERT_GT(n, 0) << "write failed: errno=" << errno << " (" << strerror(errno) << ")";
        p += n;
        size -= static_cast<size_t>(n);
    }
}

void write_check_rdx_elf(char* path, size_t path_size) {
    snprintf(path, path_size, "/tmp/exec_abi_check_rdx_%d", getpid());
    int fd = open(path, O_CREAT | O_TRUNC | O_WRONLY, 0755);
    ASSERT_GE(fd, 0) << "open(" << path << ") failed: errno=" << errno << " ("
                     << strerror(errno) << ")";

    write_all(fd, kCheckRdxElf, sizeof(kCheckRdxElf));
    ASSERT_EQ(0, close(fd)) << "close(" << path << ") failed: errno=" << errno << " ("
                            << strerror(errno) << ")";
    ASSERT_EQ(0, chmod(path, 0755)) << "chmod(" << path << ") failed: errno=" << errno << " ("
                                    << strerror(errno) << ")";
}

void write_executable(const char* path, const void* data, size_t size) {
    int fd = open(path, O_CREAT | O_TRUNC | O_WRONLY, 0755);
    ASSERT_GE(fd, 0) << "open(" << path << ") failed: errno=" << errno << " ("
                     << strerror(errno) << ")";
    write_all(fd, data, size);
    ASSERT_EQ(0, close(fd));
    ASSERT_EQ(0, chmod(path, 0755));
}

int set_test_robust_list(TestRobustListHead* head, TestRobustNode* node) {
    const pid_t tid = static_cast<pid_t>(syscall(SYS_gettid));
    node->next = reinterpret_cast<uintptr_t>(head);
    node->futex = static_cast<uint32_t>(tid);
    head->next = reinterpret_cast<uintptr_t>(node);
    head->futex_offset = offsetof(TestRobustNode, futex);
    head->list_op_pending = 0;
    return static_cast<int>(syscall(SYS_set_robust_list, head, sizeof(*head)));
}

void shared_sighand_handler(int) {}

int shared_sighand_exec_child(void* opaque) {
    auto* path = static_cast<char*>(opaque);
    char* const argv[] = {path, nullptr};
    char child_mode[] = "DRAGONOS_EXEC_ABI_SHARED_CHILD=1";
    char* const envp[] = {child_mode, nullptr};
    execve(path, argv, envp);
    return 97;
}

#endif

void ensure_tmp_dir() {
    if (mkdir("/tmp", 0755) != 0 && errno != EEXIST) {
        FAIL() << "mkdir(/tmp) failed: errno=" << errno << " (" << strerror(errno) << ")";
    }
}

}  // namespace

TEST(ExecAbi, X86_64ExecClearsRdxForProgramEntry) {
#if !defined(__x86_64__)
    GTEST_SKIP() << "x86_64-specific exec register ABI test";
#else
    ensure_tmp_dir();
#endif
}

#if defined(__x86_64__)

TEST(ExecAbi, X86_64ExecClearsRdxWhenEnvpIsNonNull) {
    ensure_tmp_dir();

    char path[128] = {};
    write_check_rdx_elf(path, sizeof(path));

    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        char arg0[] = "check-rdx";
        char env0[] = "DRAGONOS_EXEC_ABI_RDX=non-null-envp";
        char* const argv[] = {arg0, nullptr};
        char* const envp[] = {env0, nullptr};
        execve(path, argv, envp);
        _exit(errno);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0))
        << "waitpid failed: errno=" << errno << " (" << strerror(errno) << ")";
    unlink(path);

    ASSERT_TRUE(WIFEXITED(status)) << "child did not exit normally, status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status))
        << "exec entry %rdx was not cleared; exit 42 means old envp leaked into %rdx";
}

TEST(ExecAbi, SuccessfulExecCleansOldRobustList) {
    ensure_tmp_dir();
    char path[128] = {};
    snprintf(path, sizeof(path), "/tmp/exec_abi_robust_%d", getpid());
    write_executable(path, kCheckRobustListClearedElf, sizeof(kCheckRobustListClearedElf));

    auto* node = static_cast<TestRobustNode*>(
        mmap(nullptr, sizeof(TestRobustNode), PROT_READ | PROT_WRITE,
             MAP_SHARED | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, node);

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        TestRobustListHead head = {};
        if (set_test_robust_list(&head, node) != 0) {
            _exit(90);
        }
        char* const argv[] = {path, nullptr};
        char* const envp[] = {nullptr};
        execve(path, argv, envp);
        _exit(91);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    const uint32_t futex = __atomic_load_n(&node->futex, __ATOMIC_SEQ_CST);
    EXPECT_EQ(0U, futex & kFutexTidMask);
    EXPECT_NE(0U, futex & kFutexOwnerDied);

    EXPECT_EQ(0, munmap(node, sizeof(TestRobustNode)));
    EXPECT_EQ(0, unlink(path));
}

TEST(ExecAbi, SuccessfulExecToleratesReadOnlyRobustFutex) {
    ensure_tmp_dir();
    char path[128] = {};
    snprintf(path, sizeof(path), "/tmp/exec_abi_robust_ro_%d", getpid());
    write_executable(path, kCheckRobustListClearedElf, sizeof(kCheckRobustListClearedElf));

    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    auto* node = static_cast<TestRobustNode*>(
        mmap(nullptr, static_cast<size_t>(page_size), PROT_READ | PROT_WRITE,
             MAP_SHARED | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, node);

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        TestRobustListHead head = {};
        if (set_test_robust_list(&head, node) != 0) {
            _exit(96);
        }
        if (mprotect(node, static_cast<size_t>(page_size), PROT_READ) != 0) {
            _exit(97);
        }
        char* const argv[] = {path, nullptr};
        char* const envp[] = {nullptr};
        execve(path, argv, envp);
        _exit(98);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    const uint32_t futex = __atomic_load_n(&node->futex, __ATOMIC_SEQ_CST);
    EXPECT_NE(0U, futex & kFutexTidMask);
    EXPECT_EQ(0U, futex & kFutexOwnerDied);

    EXPECT_EQ(0, munmap(node, static_cast<size_t>(page_size)));
    EXPECT_EQ(0, unlink(path));
}

TEST(ExecAbi, EarlyFailedExecPreservesRobustList) {
    ensure_tmp_dir();
    char early_path[128] = {};
    snprintf(early_path, sizeof(early_path), "/tmp/exec_abi_bad_early_%d", getpid());
    static constexpr char kNotElf[] = "not an executable";
    write_executable(early_path, kNotElf, sizeof(kNotElf));

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        TestRobustListHead head = {};
        TestRobustNode node = {};
        if (set_test_robust_list(&head, &node) != 0) {
            _exit(92);
        }
        char* const argv[] = {early_path, nullptr};
        char* const envp[] = {nullptr};
        if (execve(early_path, argv, envp) == 0) {
            _exit(93);
        }
        TestRobustListHead* observed = nullptr;
        size_t observed_size = 0;
        if (syscall(SYS_get_robust_list, 0, &observed, &observed_size) != 0) {
            _exit(94);
        }
        if (observed != &head || observed_size != sizeof(head) ||
            (node.futex & kFutexOwnerDied) != 0) {
            _exit(95);
        }
        _exit(0);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));

    EXPECT_EQ(0, unlink(early_path));
}

TEST(ExecAbi, PostPointOfNoReturnExecFailureIsFatal) {
    ensure_tmp_dir();
    char path[128] = {};
    snprintf(path, sizeof(path), "/tmp/exec_abi_bad_post_ponr_%d", getpid());

    unsigned char malformed[sizeof(kCheckRdxElf)] = {};
    memcpy(malformed, kCheckRdxElf, sizeof(malformed));
    malformed[96] += 1;  // p_filesz > p_memsz, rejected after begin_new_exec().
    write_executable(path, malformed, sizeof(malformed));

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        char* const argv[] = {path, nullptr};
        char* const envp[] = {nullptr};
        execve(path, argv, envp);
        _exit(96);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    EXPECT_TRUE(WIFSIGNALED(status));
    if (WIFSIGNALED(status)) {
        EXPECT_EQ(SIGSEGV, WTERMSIG(status));
    }
    EXPECT_EQ(0, unlink(path));
}

TEST(ExecAbi, PostPonrFailureDoesNotChangeSharedSiblingHandler) {
    ensure_tmp_dir();
    char path[128] = {};
    snprintf(path, sizeof(path), "/tmp/exec_abi_bad_shared_sighand_%d", getpid());

    unsigned char malformed[sizeof(kCheckRdxElf)] = {};
    memcpy(malformed, kCheckRdxElf, sizeof(malformed));
    malformed[96] += 1;
    write_executable(path, malformed, sizeof(malformed));

    struct sigaction saved = {};
    struct sigaction custom = {};
    ASSERT_EQ(0, sigaction(SIGSEGV, nullptr, &saved));
    custom.sa_handler = shared_sighand_handler;
    sigemptyset(&custom.sa_mask);
    ASSERT_EQ(0, sigaction(SIGSEGV, &custom, nullptr));

    constexpr size_t kStackSize = 64 * 1024;
    std::vector<unsigned char> stack(kStackSize);
    pid_t child = clone(shared_sighand_exec_child, stack.data() + stack.size(),
                        CLONE_VM | CLONE_SIGHAND | SIGCHLD, path);
    int status = 0;
    int waited = child < 0 ? -1 : waitpid(child, &status, 0);
    struct sigaction observed = {};
    int query_result = sigaction(SIGSEGV, nullptr, &observed);
    int restore_result = sigaction(SIGSEGV, &saved, nullptr);
    int unlink_result = unlink(path);

    ASSERT_GE(child, 0);
    ASSERT_EQ(child, waited);
    ASSERT_TRUE(WIFSIGNALED(status));
    EXPECT_EQ(SIGSEGV, WTERMSIG(status));
    ASSERT_EQ(0, query_result);
    EXPECT_EQ(shared_sighand_handler, observed.sa_handler);
    EXPECT_EQ(0, restore_result);
    EXPECT_EQ(0, unlink_result);
}

TEST(ExecAbi, SuccessfulExecDoesNotChangeSharedSiblingHandler) {
    ASSERT_NE('\0', g_self_path[0]) << "self executable path was not initialized";

    struct sigaction saved = {};
    struct sigaction custom = {};
    ASSERT_EQ(0, sigaction(SIGUSR1, nullptr, &saved));
    custom.sa_handler = shared_sighand_handler;
    sigemptyset(&custom.sa_mask);
    ASSERT_EQ(0, sigaction(SIGUSR1, &custom, nullptr));

    constexpr size_t kStackSize = 64 * 1024;
    std::vector<unsigned char> stack(kStackSize);
    pid_t child = clone(shared_sighand_exec_child, stack.data() + stack.size(),
                        CLONE_VM | CLONE_SIGHAND | SIGCHLD, g_self_path);
    int status = 0;
    int waited = child < 0 ? -1 : waitpid(child, &status, 0);
    struct sigaction observed = {};
    int query_result = sigaction(SIGUSR1, nullptr, &observed);
    int restore_result = sigaction(SIGUSR1, &saved, nullptr);

    ASSERT_GE(child, 0);
    ASSERT_EQ(child, waited);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    ASSERT_EQ(0, query_result);
    EXPECT_EQ(shared_sighand_handler, observed.sa_handler);
    EXPECT_EQ(0, restore_result);
}

#endif

TEST(ExecAbi, AuxvUidGidFollowCredentialsAtExec) {
    ASSERT_NE('\0', g_self_path[0]) << "self executable path was not initialized";

    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        if (setgid(1234) != 0 || setuid(1234) != 0) {
            _exit(120);
        }

        char env0[] = "DRAGONOS_EXEC_ABI_CHECK_AUXV=1";
        char* const argv[] = {g_self_path, nullptr};
        char* const envp[] = {env0, nullptr};
        execve(g_self_path, argv, envp);
        _exit(errno);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0))
        << "waitpid failed: errno=" << errno << " (" << strerror(errno) << ")";

    ASSERT_TRUE(WIFEXITED(status)) << "child did not exit normally, status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status))
        << "exec auxv uid/gid entries did not match process credentials";
}

int main(int argc, char** argv) {
    if (getenv("DRAGONOS_EXEC_ABI_SHARED_CHILD") != nullptr) {
        return 0;
    }
    if (getenv("DRAGONOS_EXEC_ABI_CHECK_AUXV") != nullptr) {
        return check_auxv_credentials();
    }

    ssize_t path_len = readlink("/proc/self/exe", g_self_path, sizeof(g_self_path) - 1);
    if (path_len > 0) {
        g_self_path[path_len] = '\0';
    } else if (argc > 0 && argv[0] != nullptr) {
        strncpy(g_self_path, argv[0], sizeof(g_self_path) - 1);
        g_self_path[sizeof(g_self_path) - 1] = '\0';
    }

    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
