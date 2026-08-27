// PTRACE_GET_SYSCALL_INFO entry/exit stop regression test.

#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <chrono>

#include <gtest/gtest.h>

#ifndef PTRACE_TRACEME
#define PTRACE_TRACEME 0
#endif
#ifndef PTRACE_CONT
#define PTRACE_CONT 7
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

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
