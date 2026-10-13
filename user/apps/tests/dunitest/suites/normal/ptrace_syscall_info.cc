// PTRACE_GET_SYSCALL_INFO entry/exit stop regression test.

#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <sys/user.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <chrono>

#include <gtest/gtest.h>

#ifndef PTRACE_TRACEME
#define PTRACE_TRACEME 0
#endif
#ifndef PTRACE_CONT
#define PTRACE_CONT 7
#endif
#ifndef PTRACE_GETREGS
#define PTRACE_GETREGS 12
#endif
#ifndef PTRACE_SETREGS
#define PTRACE_SETREGS 13
#endif
#ifndef PTRACE_SYSCALL
#define PTRACE_SYSCALL 24
#endif
#ifndef PTRACE_SETOPTIONS
#define PTRACE_SETOPTIONS 0x4200
#endif
#ifndef PTRACE_GET_SYSCALL_INFO
#define PTRACE_GET_SYSCALL_INFO 0x420e
#endif
#ifndef PTRACE_O_TRACESYSGOOD
#define PTRACE_O_TRACESYSGOOD 0x00000001
#endif

namespace {

constexpr uint8_t kSyscallInfoEntry = 1;
constexpr uint8_t kSyscallInfoExit = 2;
constexpr uint32_t kAuditArchX86_64 = 0xc000003e;

struct PtraceSyscallInfo {
    uint8_t op;
    uint8_t pad[3];
    uint32_t arch;
    uint64_t instruction_pointer;
    uint64_t stack_pointer;
    union {
        struct {
            uint64_t nr;
            uint64_t args[6];
        } entry;
        struct {
            int64_t rval;
            uint8_t is_error;
        } exit;
    } data;
};

static_assert(sizeof(PtraceSyscallInfo) >= 80,
              "syscall-info buffer must hold Linux entry payload");

long ptrace_call(long request, pid_t pid, unsigned long addr,
                 unsigned long data) {
    return syscall(SYS_ptrace, request, pid, addr, data);
}

bool WaitPidUntil(pid_t child, int* status,
                  std::chrono::milliseconds timeout = std::chrono::seconds(2)) {
    const auto deadline = std::chrono::steady_clock::now() + timeout;
    for (;;) {
        const pid_t result = waitpid(child, status, WNOHANG);
        if (result == child) {
            return true;
        }
        if (result < 0 && errno != EINTR) {
            return false;
        }
        if (std::chrono::steady_clock::now() >= deadline) {
            errno = ETIMEDOUT;
            return false;
        }
        sched_yield();
    }
}

class ChildGuard {
public:
    explicit ChildGuard(pid_t child) : child_(child) {}
    ~ChildGuard() {
        if (child_ > 0) {
            kill(child_, SIGKILL);
            int status = 0;
            WaitPidUntil(child_, &status);
        }
    }
    void Release() { child_ = -1; }

private:
    pid_t child_;
};

TEST(PtraceSyscallInfo, TracesysgoodReportsMatchingEntryAndExit) {
#if !defined(__x86_64__)
    GTEST_SKIP() << "DragonOS exposes PTRACE_GET_SYSCALL_INFO on x86_64";
#else
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) {
            _exit(70);
        }
        raise(SIGSTOP);
        const long result = syscall(SYS_getpid);
        _exit(result == getpid() ? 0 : 71);
    }
    ChildGuard guard(child);

    int status = 0;
    ASSERT_TRUE(WaitPidUntil(child, &status));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));
    ASSERT_EQ(0, ptrace_call(PTRACE_SETOPTIONS, child, 0,
                             PTRACE_O_TRACESYSGOOD));

    bool found_getpid_entry = false;
    for (int stop_count = 0; stop_count < 16 && !found_getpid_entry;
         ++stop_count) {
        ASSERT_EQ(0, ptrace_call(PTRACE_SYSCALL, child, 0, 0));
        ASSERT_TRUE(WaitPidUntil(child, &status));
        ASSERT_TRUE(WIFSTOPPED(status));
        ASSERT_EQ(SIGTRAP | 0x80, WSTOPSIG(status));

        PtraceSyscallInfo info = {};
        const long size = ptrace_call(
            PTRACE_GET_SYSCALL_INFO, child, sizeof(info),
            reinterpret_cast<unsigned long>(&info));
        ASSERT_GE(size, 0);
        ASSERT_EQ(kAuditArchX86_64, info.arch);
        ASSERT_NE(0U, info.instruction_pointer);
        ASSERT_NE(0U, info.stack_pointer);
        if (info.op == kSyscallInfoEntry && info.data.entry.nr == SYS_getpid) {
            EXPECT_EQ(80, size);
            found_getpid_entry = true;
        }
    }
    ASSERT_TRUE(found_getpid_entry);

    ASSERT_EQ(0, ptrace_call(PTRACE_SYSCALL, child, 0, 0));
    ASSERT_TRUE(WaitPidUntil(child, &status));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGTRAP | 0x80, WSTOPSIG(status));

    PtraceSyscallInfo info = {};
    const long size = ptrace_call(PTRACE_GET_SYSCALL_INFO, child, sizeof(info),
                                  reinterpret_cast<unsigned long>(&info));
    ASSERT_EQ(33, size);
    ASSERT_EQ(kSyscallInfoExit, info.op);
    ASSERT_EQ(kAuditArchX86_64, info.arch);
    ASSERT_EQ(child, info.data.exit.rval);
    ASSERT_EQ(0, info.data.exit.is_error);

    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));
    ASSERT_TRUE(WaitPidUntil(child, &status));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    guard.Release();
#endif
}

TEST(PtraceSyscallInfo, SyscallResumeDoesNotInterruptAnOtherwiseBlockingRead) {
#if !defined(__x86_64__)
    GTEST_SKIP() << "DragonOS exposes PTRACE_GET_SYSCALL_INFO on x86_64";
#else
    int pipes[2];
    ASSERT_EQ(0, pipe(pipes));
    struct PipeGuard {
        int reader;
        int writer;
        ~PipeGuard() {
            if (reader >= 0) close(reader);
            if (writer >= 0) close(writer);
        }
    } pipe_guard{pipes[0], pipes[1]};

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(pipes[1]);
        alarm(10);
        if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) _exit(74);
        raise(SIGSTOP);
        char byte = 0;
        long result = syscall(SYS_read, pipes[0], &byte, 1);
        _exit(result == 1 && byte == 'X' ? 0 : 75);
    }
    ChildGuard guard(child);
    close(pipe_guard.reader);
    pipe_guard.reader = -1;

    int status = 0;
    ASSERT_TRUE(WaitPidUntil(child, &status));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));
    ASSERT_EQ(0, ptrace_call(PTRACE_SETOPTIONS, child, 0,
                             PTRACE_O_TRACESYSGOOD));
    bool read_entry = false;
    for (int stops = 0; stops < 32 && !read_entry; ++stops) {
        ASSERT_EQ(0, ptrace_call(PTRACE_SYSCALL, child, 0, 0));
        ASSERT_TRUE(WaitPidUntil(child, &status));
        ASSERT_TRUE(WIFSTOPPED(status));
        ASSERT_EQ(SIGTRAP | 0x80, WSTOPSIG(status));
        PtraceSyscallInfo info{};
        ASSERT_GE(ptrace_call(PTRACE_GET_SYSCALL_INFO, child, sizeof(info),
                              reinterpret_cast<unsigned long>(&info)), 0);
        read_entry = info.op == kSyscallInfoEntry && info.data.entry.nr == SYS_read;
        if (read_entry) {
            EXPECT_EQ(static_cast<uint64_t>(pipes[0]), info.data.entry.args[0]);
            EXPECT_EQ(1U, info.data.entry.args[2]);
        }
    }
    ASSERT_TRUE(read_entry);
    ASSERT_EQ(0, ptrace_call(PTRACE_SYSCALL, child, 0, 0));

    // No cgroup.freeze request and no data or delivered signal exist. The
    // ptrace-stop acknowledgement must not manufacture a syscall interruption.
    const auto deadline = std::chrono::steady_clock::now() +
                          std::chrono::milliseconds(100);
    do {
        pid_t result = waitpid(child, &status, WNOHANG);
        if (result < 0 && errno == EINTR) continue;
        ASSERT_EQ(0, result) << "blocking read produced an unsolicited stop or exit";
        timespec interval{0, 1000000};
        while (nanosleep(&interval, &interval) < 0 && errno == EINTR) {}
    } while (std::chrono::steady_clock::now() < deadline);

    ASSERT_EQ(1, write(pipe_guard.writer, "X", 1));
    ASSERT_TRUE(WaitPidUntil(child, &status));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGTRAP | 0x80, WSTOPSIG(status));
    PtraceSyscallInfo info{};
    ASSERT_EQ(33, ptrace_call(PTRACE_GET_SYSCALL_INFO, child, sizeof(info),
                              reinterpret_cast<unsigned long>(&info)));
    ASSERT_EQ(kSyscallInfoExit, info.op);
    EXPECT_EQ(1, info.data.exit.rval);
    EXPECT_EQ(0, info.data.exit.is_error);
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));
    ASSERT_TRUE(WaitPidUntil(child, &status));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    guard.Release();
#endif
}

enum class RewriteExpectation {
    kPreserveTracerResult,
    kReturnEnosys,
    kExecuteGetpid,
};

void VerifySyscallRewrite(unsigned long rewritten_nr,
                          RewriteExpectation expectation) {
    constexpr unsigned long kSimulatedResult = 0x12345678UL;
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) {
            _exit(72);
        }
        raise(SIGSTOP);
        errno = 0;
        const long result = syscall(SYS_getpid);
        bool expected = false;
        switch (expectation) {
            case RewriteExpectation::kPreserveTracerResult:
                expected = result == static_cast<long>(kSimulatedResult);
                break;
            case RewriteExpectation::kReturnEnosys:
                expected = result == -1 && errno == ENOSYS;
                break;
            case RewriteExpectation::kExecuteGetpid:
                expected = result == getpid();
                break;
        }
        _exit(expected ? 0 : 73);
    }
    ChildGuard guard(child);

    int status = 0;
    ASSERT_TRUE(WaitPidUntil(child, &status));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));
    ASSERT_EQ(0, ptrace_call(PTRACE_SETOPTIONS, child, 0,
                             PTRACE_O_TRACESYSGOOD));

    bool found_getpid_entry = false;
    for (int stop_count = 0; stop_count < 16 && !found_getpid_entry;
         ++stop_count) {
        ASSERT_EQ(0, ptrace_call(PTRACE_SYSCALL, child, 0, 0));
        ASSERT_TRUE(WaitPidUntil(child, &status));
        ASSERT_TRUE(WIFSTOPPED(status));
        ASSERT_EQ(SIGTRAP | 0x80, WSTOPSIG(status));

        PtraceSyscallInfo info = {};
        ASSERT_GE(ptrace_call(PTRACE_GET_SYSCALL_INFO, child, sizeof(info),
                              reinterpret_cast<unsigned long>(&info)),
                  0);
        if (info.op != kSyscallInfoEntry ||
            info.data.entry.nr != SYS_getpid) {
            continue;
        }

        user_regs_struct regs = {};
        ASSERT_EQ(0, ptrace_call(PTRACE_GETREGS, child, 0,
                                 reinterpret_cast<unsigned long>(&regs)));
        regs.orig_rax = rewritten_nr;
        regs.rax = kSimulatedResult;
        ASSERT_EQ(0, ptrace_call(PTRACE_SETREGS, child, 0,
                                 reinterpret_cast<unsigned long>(&regs)));
        found_getpid_entry = true;
    }
    ASSERT_TRUE(found_getpid_entry);

    ASSERT_EQ(0, ptrace_call(PTRACE_SYSCALL, child, 0, 0));
    ASSERT_TRUE(WaitPidUntil(child, &status));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGTRAP | 0x80, WSTOPSIG(status));

    PtraceSyscallInfo info = {};
    ASSERT_EQ(33, ptrace_call(PTRACE_GET_SYSCALL_INFO, child, sizeof(info),
                              reinterpret_cast<unsigned long>(&info)));
    ASSERT_EQ(kSyscallInfoExit, info.op);
    int64_t expected_result = -ENOSYS;
    if (expectation == RewriteExpectation::kPreserveTracerResult) {
        expected_result = static_cast<int64_t>(kSimulatedResult);
    } else if (expectation == RewriteExpectation::kExecuteGetpid) {
        expected_result = child;
    }
    EXPECT_EQ(expected_result, info.data.exit.rval);
    EXPECT_EQ(expectation == RewriteExpectation::kReturnEnosys ? 1 : 0,
              info.data.exit.is_error);

    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));
    ASSERT_TRUE(WaitPidUntil(child, &status));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    guard.Release();
}

TEST(PtraceSyscallInfo, MinusOneSentinelPreservesTracerResult) {
#if !defined(__x86_64__)
    GTEST_SKIP() << "register rewrite is x86_64-specific";
#else
    VerifySyscallRewrite(static_cast<unsigned long>(-1),
                         RewriteExpectation::kPreserveTracerResult);
#endif
}

TEST(PtraceSyscallInfo, OtherNegativeSyscallNumberReturnsEnosys) {
#if !defined(__x86_64__)
    GTEST_SKIP() << "register rewrite is x86_64-specific";
#else
    VerifySyscallRewrite(static_cast<unsigned long>(-2),
                         RewriteExpectation::kReturnEnosys);
#endif
}

TEST(PtraceSyscallInfo, SyscallNumberUsesSignedLowWord) {
#if !defined(__x86_64__)
    GTEST_SKIP() << "register rewrite is x86_64-specific";
#else
    VerifySyscallRewrite(0x00000000ffffffffUL,
                         RewriteExpectation::kPreserveTracerResult);
    VerifySyscallRewrite(0x1234567800000000UL |
                             static_cast<unsigned long>(SYS_getpid),
                         RewriteExpectation::kExecuteGetpid);
#endif
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
