#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>
#include <vector>

namespace {

constexpr char kExecExit42[] = "--proc-self-exec-cmdline-exit42";
constexpr char kLeaderExec[] = "--proc-self-exec-cmdline-leader";
constexpr char kSiblingExec[] = "--proc-self-exec-cmdline-sibling";

bool read_all(int fd, std::string* out) {
    out->clear();
    char buf[256];
    ssize_t n = 0;
    while ((n = read(fd, buf, sizeof(buf))) > 0) {
        out->append(buf, static_cast<size_t>(n));
    }
    return n == 0;
}

std::string expected_cmdline(int argc, char** argv) {
    std::string expected;
    for (int i = 0; i < argc; ++i) {
        expected.append(argv[i]);
        expected.push_back('\0');
    }
    return expected;
}

int validate_cmdline_and_exit42(int argc, char** argv) {
    int fd = open("/proc/self/cmdline", O_RDONLY);
    if (fd < 0) {
        dprintf(STDERR_FILENO,
                "open /proc/self/cmdline failed: errno=%d (%s)\n",
                errno,
                strerror(errno));
        return 1;
    }

    std::string actual;
    const bool ok = read_all(fd, &actual);
    const int saved_errno = errno;
    close(fd);
    if (!ok) {
        dprintf(STDERR_FILENO,
                "read /proc/self/cmdline failed: errno=%d (%s)\n",
                saved_errno,
                strerror(saved_errno));
        return 1;
    }

    const std::string expected = expected_cmdline(argc, argv);
    if (actual != expected) {
        dprintf(STDERR_FILENO,
                "/proc/self/cmdline mismatch: actual_size=%zu expected_size=%zu\n",
                actual.size(),
                expected.size());
        return 1;
    }

    return 42;
}

void exec_exit42() {
    char arg0[] = "/proc/self/exe";
    char arg1[] = "--proc-self-exec-cmdline-exit42";
    char* const argv[] = {arg0, arg1, nullptr};
    char* const envp[] = {nullptr};
    execve("/proc/self/exe", argv, envp);
    _exit(errno);
}

void* pause_thread(void*) {
    for (;;) {
        pause();
    }
    return nullptr;
}

void run_leader_exec_helper() {
    pthread_t thread;
    if (pthread_create(&thread, nullptr, pause_thread, nullptr) != 0) {
        _exit(1);
    }

    exec_exit42();
}

void* sibling_exec_thread(void*) {
    exec_exit42();
    return nullptr;
}

void run_sibling_exec_helper() {
    pthread_t thread;
    if (pthread_create(&thread, nullptr, sibling_exec_thread, nullptr) != 0) {
        _exit(1);
    }

    for (;;) {
        pause();
    }
}

void expect_helper_exits_42(const char* mode) {
    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        char arg0[] = "/proc/self/exe";
        char* const argv[] = {arg0, const_cast<char*>(mode), nullptr};
        char* const envp[] = {nullptr};
        execve("/proc/self/exe", argv, envp);
        _exit(errno);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0))
        << "waitpid failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_TRUE(WIFEXITED(status)) << "child did not exit normally, status=" << status;
    EXPECT_EQ(42, WEXITSTATUS(status));
}

}  // namespace

TEST(ProcSelfExecCmdline, LeaderThreadExecKeepsProcSelfCmdlineCurrent) {
    expect_helper_exits_42(kLeaderExec);
}

TEST(ProcSelfExecCmdline, SiblingThreadExecKeepsProcSelfCmdlineCurrent) {
    expect_helper_exits_42(kSiblingExec);
}

int main(int argc, char** argv) {
    if (argc >= 2 && strcmp(argv[1], kExecExit42) == 0) {
        return validate_cmdline_and_exit42(argc, argv);
    }
    if (argc >= 2 && strcmp(argv[1], kLeaderExec) == 0) {
        run_leader_exec_helper();
        return 1;
    }
    if (argc >= 2 && strcmp(argv[1], kSiblingExec) == 0) {
        run_sibling_exec_helper();
        return 1;
    }

    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
