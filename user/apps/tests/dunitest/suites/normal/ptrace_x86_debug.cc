// x86_64 ptrace register/debug-register/#DB ABI regression tests.

#include <errno.h>
#include <elf.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <time.h>
#include <sys/ptrace.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/user.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#include <gtest/gtest.h>

#ifndef PTRACE_SEIZE
#define PTRACE_SEIZE 0x4206
#endif

namespace {

constexpr unsigned long kDr0Offset = offsetof(struct user, u_debugreg[0]);
constexpr unsigned long kDr1Offset = offsetof(struct user, u_debugreg[1]);
constexpr unsigned long kDr6Offset = offsetof(struct user, u_debugreg[6]);
constexpr unsigned long kDr7Offset = offsetof(struct user, u_debugreg[7]);
constexpr unsigned long kDrLocalEnable = 1;
constexpr unsigned long kDr1LocalEnable = 1UL << 2;
constexpr unsigned long kDrWrite = 1;
constexpr unsigned long kDrReadWrite = 3;
constexpr unsigned long kDrLen2 = 1;
constexpr unsigned long kDrLen8 = 2;
constexpr unsigned long kDrLen4 = 3;
constexpr unsigned long kDr6Breakpoint0 = 1;
constexpr unsigned long kDr6Breakpoint1 = 1UL << 1;
constexpr unsigned long kDr6SingleStep = 1UL << 14;
constexpr unsigned long kEflagsTrap = 1UL << 8;
constexpr int kDeadlineMs = 3000;

alignas(8) volatile uint64_t watched_word = 0;
volatile uint64_t execution_marker = 0;
volatile sig_atomic_t* signal_handler_marker = nullptr;

struct RetainedBreakpointState {
    int tracee_active;
    int fire_tracee;
};

__attribute__((noinline, noclone, used)) void execution_breakpoint_target() {
    asm volatile("" ::: "memory");
    execution_marker++;
    asm volatile("" ::: "memory");
}

extern "C" void coalesced_write_instruction();

extern "C" __attribute__((naked, noinline, used)) void coalesced_write_target() {
    // The global label points at the memory-writing instruction itself. Some
    // host toolchains insert ENDBR64 at a naked function entry, so using the
    // C function address would single-step that prefix instead of the write.
    asm volatile(".globl coalesced_write_instruction\n"
                 "coalesced_write_instruction:\n\t"
                 "incq (%rdi)\n\t"
                 "int3\n\t"
                 "ud2");
}

void mark_signal_handler(int) { *signal_handler_marker = 1; }

long ptrace_call(long request, pid_t pid, unsigned long addr,
                 unsigned long data) {
    return syscall(SYS_ptrace, request, pid, addr, data);
}

long peek_user(pid_t pid, unsigned long offset, unsigned long* value) {
    return ptrace_call(PTRACE_PEEKUSER, pid, offset,
                       reinterpret_cast<unsigned long>(value));
}

int64_t monotonic_millis() {
    timespec now = {};
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return -1;
    return static_cast<int64_t>(now.tv_sec) * 1000 + now.tv_nsec / 1000000;
}

pid_t waitpid_deadline(pid_t pid, int* status, int options = 0,
                       int timeout_ms = kDeadlineMs) {
    const int64_t start = monotonic_millis();
    if (start < 0) return -1;
    const int64_t deadline = start + timeout_ms;
    for (;;) {
        const pid_t result = waitpid(pid, status, options | WNOHANG);
        if (result != 0) return result;
        if (monotonic_millis() >= deadline) {
            errno = ETIMEDOUT;
            return -1;
        }
        poll(nullptr, 0, 1);
    }
}

bool running_on_dragonos() {
    utsname name = {};
    return uname(&name) == 0 && strstr(name.release, "dragonos") != nullptr;
}

bool read_byte_deadline(int fd, char* byte, int timeout_ms = kDeadlineMs) {
    const int64_t start = monotonic_millis();
    if (start < 0) return false;
    const int64_t deadline = start + timeout_ms;
    pollfd pfd = {.fd = fd, .events = POLLIN, .revents = 0};
    for (;;) {
        const int64_t now = monotonic_millis();
        if (now < 0) return false;
        if (now >= deadline) {
            errno = ETIMEDOUT;
            return false;
        }
        const int result = poll(&pfd, 1, static_cast<int>(deadline - now));
        if (result > 0) return read(fd, byte, 1) == 1;
        if (result == 0) {
            errno = ETIMEDOUT;
            return false;
        }
        if (errno != EINTR) return false;
    }
}

bool wait_shared_value(const int* value, int expected,
                       int timeout_ms = kDeadlineMs) {
    const int64_t start = monotonic_millis();
    if (start < 0) return false;
    const int64_t deadline = start + timeout_ms;
    while (__atomic_load_n(value, __ATOMIC_ACQUIRE) != expected) {
        if (monotonic_millis() >= deadline) {
            errno = ETIMEDOUT;
            return false;
        }
        sched_yield();
    }
    return true;
}

class ChildGuard {
public:
    explicit ChildGuard(pid_t pid) : pid_(pid) {}
    ChildGuard(const ChildGuard&) = delete;
    ChildGuard& operator=(const ChildGuard&) = delete;
    ~ChildGuard() {
        if (pid_ <= 0) return;
        kill(pid_, SIGKILL);
        int status = 0;
        (void)waitpid_deadline(pid_, &status, 0, 1000);
    }
    void release() { pid_ = -1; }

private:
    pid_t pid_;
};

class ScopedAffinity {
public:
    ScopedAffinity() : valid_(sched_getaffinity(0, sizeof(saved_), &saved_) == 0) {}
    ~ScopedAffinity() {
        if (valid_) (void)sched_setaffinity(0, sizeof(saved_), &saved_);
    }
    bool pin_to(int cpu) {
        if (!valid_) return false;
        cpu_set_t set;
        CPU_ZERO(&set);
        CPU_SET(cpu, &set);
        return sched_setaffinity(0, sizeof(set), &set) == 0;
    }
    const cpu_set_t& saved() const { return saved_; }
    bool valid() const { return valid_; }

private:
    cpu_set_t saved_ = {};
    bool valid_;
};

int first_cpu(const cpu_set_t& set, int after = -1) {
    for (int cpu = after + 1; cpu < CPU_SETSIZE; ++cpu) {
        if (CPU_ISSET(cpu, &set)) return cpu;
    }
    return -1;
}

int set_one_cpu(pid_t pid, int cpu) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    return sched_setaffinity(pid, sizeof(set), &set);
}

void expect_initial_stop(pid_t child) {
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));
}

void continue_and_reap(pid_t child) {
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

[[noreturn]] __attribute__((noinline)) void run_fs_gs_selector_tracee() {
    asm volatile("mov %[probe], %%rdx\n\t"
                 "mov $0x2b, %%ax\n\t"
                 "mov %%ax, %%fs\n\t"
                 "mov %%ax, %%gs\n\t"
                 "int3\n\t"
                 "xor %%edi, %%edi\n\t"
                 "mov %%fs, %%ax\n\t"
                 "cmp $0x2b, %%ax\n\t"
                 "setne %%dil\n\t"
                 "mov %%gs, %%ax\n\t"
                 "cmp $0x2b, %%ax\n\t"
                 "setne %%al\n\t"
                 "movzbl %%al, %%eax\n\t"
                 "or %%eax, %%edi\n\t"
                 "mov (%%rdx), %%rcx\n\t"
                 "mov %%fs:(%%rdx), %%rax\n\t"
                 "cmp %%rcx, %%rax\n\t"
                 "setne %%al\n\t"
                 "movzbl %%al, %%eax\n\t"
                 "or %%eax, %%edi\n\t"
                 "mov %%gs:(%%rdx), %%rax\n\t"
                 "cmp %%rcx, %%rax\n\t"
                 "setne %%al\n\t"
                 "movzbl %%al, %%eax\n\t"
                 "or %%eax, %%edi\n\t"
                 "mov $60, %%eax\n\t"
                 "syscall\n\t"
                 "ud2"
                 :
                 : [probe] "r"(&watched_word)
                 : "rax", "rdx", "rdi", "rcx", "r11", "memory");
    __builtin_unreachable();
}

[[noreturn]] __attribute__((noinline)) void run_zero_data_selectors_tracee(
    bool check_bases) {
    asm volatile("int3\n\t"
                 "xor %%edi, %%edi\n\t"
                 "mov %%ds, %%ax\n\t"
                 "test %%ax, %%ax\n\t"
                 "setne %%dil\n\t"
                 "mov %%es, %%ax\n\t"
                 "test %%ax, %%ax\n\t"
                 "setne %%al\n\t"
                 "movzbl %%al, %%eax\n\t"
                 "or %%eax, %%edi\n\t"
                 "mov %%fs, %%ax\n\t"
                 "test %%ax, %%ax\n\t"
                 "setne %%al\n\t"
                 "movzbl %%al, %%eax\n\t"
                 "or %%eax, %%edi\n\t"
                 "mov %%gs, %%ax\n\t"
                 "test %%ax, %%ax\n\t"
                 "setne %%al\n\t"
                 "movzbl %%al, %%eax\n\t"
                 "or %%eax, %%edi\n\t"
                 "test %[check_bases], %[check_bases]\n\t"
                 "jz 1f\n\t"
                 "mov %[probe], %%rdx\n\t"
                 "mov (%%rdx), %%rcx\n\t"
                 "mov %%fs:(%%rdx), %%rax\n\t"
                 "cmp %%rcx, %%rax\n\t"
                 "setne %%al\n\t"
                 "movzbl %%al, %%eax\n\t"
                 "or %%eax, %%edi\n\t"
                 "mov %%gs:(%%rdx), %%rax\n\t"
                 "cmp %%rcx, %%rax\n\t"
                 "setne %%al\n\t"
                 "movzbl %%al, %%eax\n\t"
                 "or %%eax, %%edi\n\t"
                 "1:\n\t"
                 "mov $60, %%eax\n\t"
                 "syscall\n\t"
                 "ud2"
                 :
                 : [probe] "r"(&watched_word), [check_bases] "r"(check_bases)
                 : "rax", "rdx", "rdi", "rcx", "r11", "memory");
    __builtin_unreachable();
}

TEST(PtraceX86Debug, GeneralRegisterWritesStopAtInvalidSegmentSelector) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) _exit(10);
        raise(SIGSTOP);
        _exit(0);
    }

    expect_initial_stop(child);
    user_regs_struct regs = {};
    ASSERT_EQ(0, ptrace_call(PTRACE_GETREGS, child, 0,
                             reinterpret_cast<unsigned long>(&regs)));
    const user_regs_struct baseline = regs;
    for (int selector = 0; selector < 4; ++selector) {
        regs = baseline;
        regs.r15 ^= 0x12345678UL + selector;
        switch (selector) {
            case 0: regs.ds = 0x10; break;
            case 1: regs.es = 0x10; break;
            case 2: regs.fs = 0x10; break;
            case 3: regs.gs = 0x10; break;
        }
        errno = 0;
        EXPECT_EQ(-1, ptrace_call(PTRACE_SETREGS, child, 0,
                                  reinterpret_cast<unsigned long>(&regs)));
        EXPECT_EQ(EIO, errno);

        user_regs_struct actual = {};
        ASSERT_EQ(0, ptrace_call(PTRACE_GETREGS, child, 0,
                                 reinterpret_cast<unsigned long>(&actual)));
        // Linux genregs_set() commits preceding words before putreg() rejects
        // the invalid selector; it does not make SETREGS atomic.
        EXPECT_EQ(regs.r15, actual.r15);
        EXPECT_EQ(selector == 0 ? baseline.ds : regs.ds, actual.ds);
        EXPECT_EQ(selector == 1 ? baseline.es : regs.es, actual.es);
        EXPECT_EQ(selector == 2 ? baseline.fs : regs.fs, actual.fs);
        EXPECT_EQ(selector == 3 ? baseline.gs : regs.gs, actual.gs);
        ASSERT_EQ(0, ptrace_call(PTRACE_SETREGS, child, 0,
                                 reinterpret_cast<unsigned long>(&baseline)));
    }

    regs = baseline;
    regs.cs = 0;
    errno = 0;
    EXPECT_EQ(-1, ptrace_call(PTRACE_SETREGS, child, 0,
                              reinterpret_cast<unsigned long>(&regs)));
    EXPECT_EQ(EIO, errno);

    regs = baseline;
    regs.ds = 0xffffffffffff002bUL;
    regs.es = 0x123400000000002bUL;
    ASSERT_EQ(0, ptrace_call(PTRACE_SETREGS, child, 0,
                             reinterpret_cast<unsigned long>(&regs)));
    user_regs_struct truncated = {};
    ASSERT_EQ(0, ptrace_call(PTRACE_GETREGS, child, 0,
                             reinterpret_cast<unsigned long>(&truncated)));
    EXPECT_EQ(0x2bUL, truncated.ds);
    EXPECT_EQ(0x2bUL, truncated.es);

    ASSERT_EQ(0, ptrace_call(PTRACE_SETREGS, child, 0,
                             reinterpret_cast<unsigned long>(&baseline)));
    continue_and_reap(child);
}

TEST(PtraceX86Debug, FsGsSelectorsRoundTripAcrossStopAndResume) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) _exit(10);
        raise(SIGSTOP);
        run_fs_gs_selector_tracee();
    }
    ChildGuard child_guard(child);

    int status = 0;
    ASSERT_EQ(child, waitpid_deadline(child, &status));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));

    ASSERT_EQ(child, waitpid_deadline(child, &status));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGTRAP, WSTOPSIG(status));
    user_regs_struct regs = {};
    ASSERT_EQ(0, ptrace_call(PTRACE_GETREGS, child, 0,
                             reinterpret_cast<unsigned long>(&regs)));
    EXPECT_EQ(0x2bUL, regs.fs);
    EXPECT_EQ(0x2bUL, regs.gs);

    // On a legacy CPU, a real selector supplies the descriptor base and the
    // saved MSR base must be ignored. On FSGSBASE CPUs the explicit base is
    // authoritative, so reset it after observing that mode before resuming.
    constexpr unsigned long kProbeBase = 0x100000;
    regs.fs_base = kProbeBase;
    regs.gs_base = kProbeBase;
    ASSERT_EQ(0, ptrace_call(PTRACE_SETREGS, child, 0,
                             reinterpret_cast<unsigned long>(&regs)));
    user_regs_struct observed = {};
    ASSERT_EQ(0, ptrace_call(PTRACE_GETREGS, child, 0,
                             reinterpret_cast<unsigned long>(&observed)));
    EXPECT_EQ(observed.fs_base, observed.gs_base);
    if (observed.fs_base != 0) {
        EXPECT_EQ(kProbeBase, observed.fs_base);
        regs.fs_base = 0;
        regs.gs_base = 0;
        ASSERT_EQ(0, ptrace_call(PTRACE_SETREGS, child, 0,
                                 reinterpret_cast<unsigned long>(&regs)));
    }
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));
    ASSERT_EQ(child, waitpid_deadline(child, &status));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    child_guard.release();
}

TEST(PtraceX86Debug, MissingUserDataSegmentDescriptorsAreClearedAtResume) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        const bool dragonos = running_on_dragonos();
        if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) _exit(10);
        run_zero_data_selectors_tracee(dragonos);
    }
    ChildGuard child_guard(child);

    int status = 0;
    ASSERT_EQ(child, waitpid_deadline(child, &status));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGTRAP, WSTOPSIG(status));
    user_regs_struct regs = {};
    ASSERT_EQ(0, ptrace_call(PTRACE_GETREGS, child, 0,
                             reinterpret_cast<unsigned long>(&regs)));
    // DragonOS GDT slot 9 is not present. RPL 3 passes Linux's ptrace input
    // validation, while the restore-time #NP fixup must clear each selector.
    regs.ds = 0x4b;
    regs.es = 0x4b;
    regs.fs = 0x4b;
    regs.gs = 0x4b;
    if (running_on_dragonos()) {
        // DragonOS currently runs with CR4.FSGSBASE clear. A failed legacy
        // selector load must not turn this ignored base into an active one.
        regs.fs_base = 0x100000;
        regs.gs_base = 0x100000;
    }
    ASSERT_EQ(0, ptrace_call(PTRACE_SETREGS, child, 0,
                             reinterpret_cast<unsigned long>(&regs)));
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));
    ASSERT_EQ(child, waitpid_deadline(child, &status));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    child_guard.release();
}

TEST(PtraceX86Debug, MissingCodeAndStackDescriptorsFaultTheTracee) {
    for (bool invalid_cs : {false, true}) {
        pid_t child = fork();
        ASSERT_GE(child, 0);
        if (child == 0) {
            if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) _exit(10);
            raise(SIGSTOP);
            _exit(0);
        }
        ChildGuard child_guard(child);

        int status = 0;
        ASSERT_EQ(child, waitpid_deadline(child, &status));
        ASSERT_TRUE(WIFSTOPPED(status));
        user_regs_struct regs = {};
        ASSERT_EQ(0, ptrace_call(PTRACE_GETREGS, child, 0,
                                 reinterpret_cast<unsigned long>(&regs)));
        if (invalid_cs) {
            regs.cs = 0x4b;
        } else {
            regs.ss = 0x4b;
        }
        ASSERT_EQ(0, ptrace_call(PTRACE_SETREGS, child, 0,
                                 reinterpret_cast<unsigned long>(&regs)));
        ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));

        ASSERT_EQ(child, waitpid_deadline(child, &status));
        ASSERT_TRUE(WIFSTOPPED(status)) << "status=0x" << std::hex << status;
        // DragonOS GDT slot 9 is an all-zero reserved descriptor. Its invalid
        // type is rejected as #GP before the present-bit check, so DragonOS
        // must report SIGSEGV. A host Linux GDT layout may instead make 0x4b a
        // valid-but-not-present descriptor and report SIGBUS.
        const int fault_signal = WSTOPSIG(status);
        if (running_on_dragonos()) {
            EXPECT_EQ(SIGSEGV, fault_signal);
        } else {
            EXPECT_TRUE(fault_signal == SIGSEGV || fault_signal == SIGBUS);
        }
        user_regs_struct fault_regs = {};
        ASSERT_EQ(0, ptrace_call(PTRACE_GETREGS, child, 0,
                                 reinterpret_cast<unsigned long>(&fault_regs)));
        EXPECT_EQ(0x4bUL, invalid_cs ? fault_regs.cs : fault_regs.ss);

        // Suppressing the delivery must not silently repair the bad iret
        // target. Returning retries iretq and reports the same fault/frame.
        ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));
        ASSERT_EQ(child, waitpid_deadline(child, &status));
        ASSERT_TRUE(WIFSTOPPED(status)) << "status=0x" << std::hex << status;
        EXPECT_EQ(fault_signal, WSTOPSIG(status));
        ASSERT_EQ(0, ptrace_call(PTRACE_GETREGS, child, 0,
                                 reinterpret_cast<unsigned long>(&fault_regs)));
        EXPECT_EQ(0x4bUL, invalid_cs ? fault_regs.cs : fault_regs.ss);

        ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, fault_signal));
        ASSERT_EQ(child, waitpid_deadline(child, &status));
        ASSERT_TRUE(WIFSIGNALED(status));
        EXPECT_EQ(fault_signal, WTERMSIG(status));
        child_guard.release();
    }
}

TEST(PtraceX86Debug, RegsetRequiresWordAlignedPrefixLength) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) _exit(10);
        raise(SIGSTOP);
        _exit(0);
    }
    ChildGuard child_guard(child);

    int status = 0;
    ASSERT_EQ(child, waitpid_deadline(child, &status));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));

    user_regs_struct baseline = {};
    ASSERT_EQ(0, ptrace_call(PTRACE_GETREGS, child, 0,
                             reinterpret_cast<unsigned long>(&baseline)));
    user_regs_struct candidate = baseline;
    candidate.r15 ^= 0x13579bdfUL;
    candidate.r14 ^= 0x2468ace0UL;

    const size_t valid_lengths[] = {
        0,
        sizeof(unsigned long),
        2 * sizeof(unsigned long),
        sizeof(user_regs_struct),
        sizeof(user_regs_struct) + 64,
    };
    for (size_t requested : valid_lengths) {
        ASSERT_EQ(0, ptrace_call(PTRACE_SETREGS, child, 0,
                                 reinterpret_cast<unsigned long>(&baseline)));
        iovec iov = {
            .iov_base = &candidate,
            .iov_len = requested,
        };
        ASSERT_EQ(0, ptrace_call(PTRACE_SETREGSET, child, NT_PRSTATUS,
                                 reinterpret_cast<unsigned long>(&iov)))
            << "requested=" << requested << " errno=" << errno;
        const size_t copied =
            requested < sizeof(candidate) ? requested : sizeof(candidate);
        EXPECT_EQ(copied, iov.iov_len);

        user_regs_struct actual = {};
        ASSERT_EQ(0, ptrace_call(PTRACE_GETREGS, child, 0,
                                 reinterpret_cast<unsigned long>(&actual)));
        user_regs_struct expected = baseline;
        memcpy(&expected, &candidate, copied);
        EXPECT_EQ(0, memcmp(&expected, &actual, sizeof(actual)))
            << "requested=" << requested;
    }

    ASSERT_EQ(0, ptrace_call(PTRACE_SETREGS, child, 0,
                             reinterpret_cast<unsigned long>(&baseline)));
    for (size_t requested : valid_lengths) {
        user_regs_struct output = {};
        iovec iov = {
            .iov_base = &output,
            .iov_len = requested,
        };
        ASSERT_EQ(0, ptrace_call(PTRACE_GETREGSET, child, NT_PRSTATUS,
                                 reinterpret_cast<unsigned long>(&iov)))
            << "requested=" << requested << " errno=" << errno;
        const size_t copied =
            requested < sizeof(output) ? requested : sizeof(output);
        EXPECT_EQ(copied, iov.iov_len);
        EXPECT_EQ(0, memcmp(&baseline, &output, copied))
            << "requested=" << requested;
    }

    for (size_t requested :
         {size_t{1}, size_t{4}, size_t{7}, size_t{9}, size_t{12}}) {
        ASSERT_EQ(0, ptrace_call(PTRACE_SETREGS, child, 0,
                                 reinterpret_cast<unsigned long>(&baseline)));
        iovec iov = {
            .iov_base = &candidate,
            .iov_len = requested,
        };
        errno = 0;
        EXPECT_EQ(-1, ptrace_call(PTRACE_SETREGSET, child, NT_PRSTATUS,
                                  reinterpret_cast<unsigned long>(&iov)))
            << "requested=" << requested;
        EXPECT_EQ(EINVAL, errno) << "requested=" << requested;
        EXPECT_EQ(requested, iov.iov_len);

        user_regs_struct actual = {};
        ASSERT_EQ(0, ptrace_call(PTRACE_GETREGS, child, 0,
                                 reinterpret_cast<unsigned long>(&actual)));
        EXPECT_EQ(0, memcmp(&baseline, &actual, sizeof(actual)))
            << "failed SETREGSET modified registers for requested=" << requested;

        user_regs_struct output = {};
        memset(&output, 0xa5, sizeof(output));
        const user_regs_struct unchanged = output;
        iov = {
            .iov_base = &output,
            .iov_len = requested,
        };
        errno = 0;
        EXPECT_EQ(-1, ptrace_call(PTRACE_GETREGSET, child, NT_PRSTATUS,
                                  reinterpret_cast<unsigned long>(&iov)))
            << "requested=" << requested;
        EXPECT_EQ(EINVAL, errno) << "requested=" << requested;
        EXPECT_EQ(requested, iov.iov_len);
        EXPECT_EQ(0, memcmp(&unchanged, &output, sizeof(output)))
            << "failed GETREGSET modified output for requested=" << requested;
    }

    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));
    ASSERT_EQ(child, waitpid_deadline(child, &status));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    child_guard.release();
}

TEST(PtraceX86Debug, Dr7LengthValidationUsesArchitecturalEncoding) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) _exit(10);
        raise(SIGSTOP);
        _exit(0);
    }

    expect_initial_stop(child);
    const auto address = reinterpret_cast<unsigned long>(&watched_word);
    ASSERT_EQ(0, ptrace_call(PTRACE_POKEUSER, child, kDr0Offset, address));

    for (unsigned long len : {kDrLen2, kDrLen4, kDrLen8}) {
        const unsigned long dr7 =
            kDrLocalEnable | ((kDrWrite | (len << 2)) << 16);
        EXPECT_EQ(0, ptrace_call(PTRACE_POKEUSER, child, kDr7Offset, dr7));
    }

    errno = 0;
    EXPECT_EQ(-1, ptrace_call(PTRACE_POKEUSER, child, kDr7Offset,
                              kDrLocalEnable | (2UL << 16)));
    EXPECT_EQ(EINVAL, errno);

    errno = 0;
    EXPECT_EQ(-1, ptrace_call(PTRACE_POKEUSER, child, kDr7Offset,
                              kDrLocalEnable | ((kDrLen2 << 2) << 16)));
    EXPECT_EQ(EINVAL, errno);

    const unsigned long len2_write =
        kDrLocalEnable | ((kDrWrite | (kDrLen2 << 2)) << 16);
    ASSERT_EQ(0, ptrace_call(PTRACE_POKEUSER, child, kDr7Offset, len2_write));
    errno = 0;
    EXPECT_EQ(-1, ptrace_call(PTRACE_POKEUSER, child, kDr0Offset, address + 1));
    EXPECT_EQ(EINVAL, errno);

    ASSERT_EQ(0, ptrace_call(PTRACE_POKEUSER, child, kDr7Offset, 0));
    continue_and_reap(child);
}

TEST(PtraceX86Debug, SingleStepReportsTrapTrace) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) _exit(10);
        raise(SIGSTOP);
        asm volatile("nop" ::: "memory");
        _exit(0);
    }

    expect_initial_stop(child);
    ASSERT_EQ(0, ptrace_call(PTRACE_SINGLESTEP, child, 0, 0));
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGTRAP, WSTOPSIG(status));
    siginfo_t info = {};
    ASSERT_EQ(0, ptrace_call(PTRACE_GETSIGINFO, child, 0,
                             reinterpret_cast<unsigned long>(&info)));
    EXPECT_EQ(TRAP_TRACE, info.si_code);
    continue_and_reap(child);
}

TEST(PtraceX86Debug, SignalFrameStopsTracerBeforeHandlerRuns) {
    auto* marker = static_cast<volatile sig_atomic_t*>(
        mmap(nullptr, sizeof(sig_atomic_t), PROT_READ | PROT_WRITE,
             MAP_SHARED | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, marker);
    *marker = 0;
    signal_handler_marker = marker;

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        struct sigaction action = {};
        action.sa_handler = mark_signal_handler;
        sigemptyset(&action.sa_mask);
        if (sigaction(SIGUSR1, &action, nullptr) != 0) _exit(11);
        if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) _exit(10);
        raise(SIGSTOP);
        asm volatile("nop" ::: "memory");
        _exit(*marker == 1 ? 0 : 12);
    }

    expect_initial_stop(child);
    ASSERT_EQ(0, ptrace_call(PTRACE_SINGLESTEP, child, 0, SIGUSR1));
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGTRAP, WSTOPSIG(status));
    // Linux constructs the handler frame, clears forced TF, and performs the
    // synthetic ptrace stop before executing the handler's first instruction.
    EXPECT_EQ(0, *marker);
    siginfo_t info = {};
    ASSERT_EQ(0, ptrace_call(PTRACE_GETSIGINFO, child, 0,
                             reinterpret_cast<unsigned long>(&info)));
    EXPECT_EQ(SIGTRAP, info.si_code);
    continue_and_reap(child);
    EXPECT_EQ(1, *marker);
    EXPECT_EQ(0, munmap(const_cast<sig_atomic_t*>(marker), sizeof(sig_atomic_t)));
}

TEST(PtraceX86Debug, HardwareWatchpointReportsDr6AndTrapHwbkpt) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) _exit(10);
        raise(SIGSTOP);
        watched_word++;
        _exit(0);
    }

    expect_initial_stop(child);
    const auto address = reinterpret_cast<unsigned long>(&watched_word);
    ASSERT_EQ(0, ptrace_call(PTRACE_POKEUSER, child, kDr0Offset, address));
    const unsigned long dr7 =
        kDrLocalEnable | ((kDrWrite | (kDrLen8 << 2)) << 16);
    ASSERT_EQ(0, ptrace_call(PTRACE_POKEUSER, child, kDr7Offset, dr7));
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGTRAP, WSTOPSIG(status));
    siginfo_t info = {};
    ASSERT_EQ(0, ptrace_call(PTRACE_GETSIGINFO, child, 0,
                             reinterpret_cast<unsigned long>(&info)));
    EXPECT_EQ(TRAP_HWBKPT, info.si_code);
    unsigned long dr6 = 0;
    ASSERT_EQ(0, peek_user(child, kDr6Offset, &dr6));
    EXPECT_NE(0UL, dr6 & 1UL);

    ASSERT_EQ(0, ptrace_call(PTRACE_POKEUSER, child, kDr7Offset, 0));
    continue_and_reap(child);
}

TEST(PtraceX86Debug, KernelCopyWatchpointThenExecutionBreakpointUsesRf) {
    int data_pipe[2] = {};
    ASSERT_EQ(0, pipe(data_pipe));
    execution_marker = 0;
    watched_word = 0x5a5a5a5a5a5a5a5aULL;

    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(data_pipe[0]);
        if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) _exit(10);
        raise(SIGSTOP);
        if (write(data_pipe[1], const_cast<const uint64_t*>(&watched_word),
                  sizeof(watched_word)) != static_cast<ssize_t>(sizeof(watched_word))) {
            _exit(11);
        }
        execution_breakpoint_target();
        _exit(execution_marker == 1 ? 0 : 12);
    }
    ChildGuard child_guard(child);
    close(data_pipe[1]);

    int status = 0;
    ASSERT_EQ(child, waitpid_deadline(child, &status));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));

    const auto watched_address = reinterpret_cast<unsigned long>(&watched_word);
    ASSERT_EQ(0, ptrace_call(PTRACE_POKEUSER, child, kDr0Offset, watched_address));
    const auto execution_address =
        reinterpret_cast<unsigned long>(&execution_breakpoint_target);
    ASSERT_EQ(0, ptrace_call(PTRACE_POKEUSER, child, kDr1Offset, execution_address));
    const unsigned long read_write_dr7 =
        kDrLocalEnable | kDr1LocalEnable |
        ((kDrReadWrite | (kDrLen8 << 2)) << 16);
    ASSERT_EQ(0, ptrace_call(PTRACE_POKEUSER, child, kDr7Offset, read_write_dr7));
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));

    // Linux 6.6 accumulates a kernel-mode data-watchpoint hit in virtual
    // DR6 without stopping at the syscall return. The next stop is the DR1
    // user execution breakpoint; exc_debug_user resets virtual DR6 before
    // publishing the current B1 cause.
    ASSERT_EQ(child, waitpid_deadline(child, &status));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGTRAP, WSTOPSIG(status));
    siginfo_t info = {};
    ASSERT_EQ(0, ptrace_call(PTRACE_GETSIGINFO, child, 0,
                             reinterpret_cast<unsigned long>(&info)));
    EXPECT_EQ(TRAP_HWBKPT, info.si_code);
    unsigned long dr6 = 0;
    ASSERT_EQ(0, peek_user(child, kDr6Offset, &dr6));
    EXPECT_EQ(kDr6Breakpoint1,
              dr6 & (kDr6Breakpoint0 | kDr6Breakpoint1));
    char byte = 0;
    ASSERT_TRUE(read_byte_deadline(data_pipe[0], &byte));

    // Keep only the DR1 execution breakpoint armed. RF must let the faulting
    // instruction execute once rather than immediately retriggering #DB.
    ASSERT_EQ(0, ptrace_call(PTRACE_POKEUSER, child, kDr7Offset,
                             kDr1LocalEnable));
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));
    ASSERT_EQ(child, waitpid_deadline(child, &status));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    child_guard.release();
    close(data_pipe[0]);
}

TEST(PtraceX86Debug,
     RetainedBreakpointDoesNotLeakAndBelongsToReattachedSession) {
    ScopedAffinity affinity;
    ASSERT_TRUE(affinity.valid()) << strerror(errno);
    const int tracee_cpu = first_cpu(affinity.saved());
    const int tracer_cpu = first_cpu(affinity.saved(), tracee_cpu);
    if (tracee_cpu < 0 || tracer_cpu < 0) {
        GTEST_SKIP() << "requires two available CPUs";
    }

    auto* state = static_cast<RetainedBreakpointState*>(
        mmap(nullptr, sizeof(RetainedBreakpointState), PROT_READ | PROT_WRITE,
             MAP_SHARED | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, state);
    memset(state, 0, sizeof(*state));
    watched_word = 0;

    const pid_t tracee = fork();
    ASSERT_GE(tracee, 0);
    if (tracee == 0) {
        if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) _exit(10);
        raise(SIGSTOP);
        __atomic_store_n(&state->tracee_active, 1, __ATOMIC_RELEASE);
        while (__atomic_load_n(&state->fire_tracee, __ATOMIC_ACQUIRE) == 0) {
            asm volatile("pause" ::: "memory");
        }
        watched_word++;
        _exit(0);
    }
    ChildGuard tracee_guard(tracee);

    int status = 0;
    ASSERT_EQ(tracee, waitpid_deadline(tracee, &status));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(0, set_one_cpu(tracee, tracee_cpu)) << strerror(errno);

    int bystander_release[2] = {};
    ASSERT_EQ(0, pipe(bystander_release));
    const pid_t bystander = fork();
    ASSERT_GE(bystander, 0);
    if (bystander == 0) {
        close(bystander_release[1]);
        char command = 0;
        if (read(bystander_release[0], &command, 1) != 1) _exit(20);
        watched_word++;
        _exit(0);
    }
    ChildGuard bystander_guard(bystander);
    close(bystander_release[0]);
    ASSERT_EQ(0, set_one_cpu(bystander, tracee_cpu)) << strerror(errno);
    ASSERT_TRUE(affinity.pin_to(tracer_cpu)) << strerror(errno);

    const auto watched_address = reinterpret_cast<unsigned long>(&watched_word);
    ASSERT_EQ(0, ptrace_call(PTRACE_POKEUSER, tracee, kDr0Offset, watched_address));
    const unsigned long dr7 =
        kDrLocalEnable | ((kDrWrite | (kDrLen8 << 2)) << 16);
    ASSERT_EQ(0, ptrace_call(PTRACE_POKEUSER, tracee, kDr7Offset, dr7));
    ASSERT_EQ(0, ptrace_call(PTRACE_DETACH, tracee, 0, 0));
    ASSERT_TRUE(wait_shared_value(&state->tracee_active, 1));

    // The running task still has debug state installed from the detached
    // session. A running SEIZE creates a new ownership generation without a
    // context switch on the tracee CPU.
    ASSERT_EQ(0, ptrace_call(PTRACE_SEIZE, tracee, 0, 0));
    __atomic_store_n(&state->fire_tracee, 1, __ATOMIC_RELEASE);
    ASSERT_EQ(tracee, waitpid_deadline(tracee, &status));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGTRAP, WSTOPSIG(status));
    siginfo_t info = {};
    ASSERT_EQ(0, ptrace_call(PTRACE_GETSIGINFO, tracee, 0,
                             reinterpret_cast<unsigned long>(&info)));
    EXPECT_EQ(TRAP_HWBKPT, info.si_code);
    unsigned long dr6 = 0;
    ASSERT_EQ(0, peek_user(tracee, kDr6Offset, &dr6));
    EXPECT_NE(0UL, dr6 & kDr6Breakpoint0);

    // DR7 was disabled on #DB entry and the tracee is stopped. Scheduling an
    // unrelated task with the same virtual address on the same CPU must not
    // inherit the retained breakpoint.
    const char command = 1;
    ASSERT_EQ(1, write(bystander_release[1], &command, 1));
    ASSERT_EQ(bystander, waitpid_deadline(bystander, &status));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    bystander_guard.release();
    close(bystander_release[1]);

    ASSERT_EQ(0, ptrace_call(PTRACE_POKEUSER, tracee, kDr7Offset, 0));
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, tracee, 0, 0));
    ASSERT_EQ(tracee, waitpid_deadline(tracee, &status));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    tracee_guard.release();
    EXPECT_EQ(0, munmap(state, sizeof(*state)));
}

TEST(PtraceX86Debug, SingleStepAndWatchpointCausesAreCoalesced) {
    watched_word = 0;
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) _exit(10);
        raise(SIGSTOP);
        _exit(0);
    }
    ChildGuard child_guard(child);

    int status = 0;
    ASSERT_EQ(child, waitpid_deadline(child, &status));
    ASSERT_TRUE(WIFSTOPPED(status));
    user_regs_struct saved = {};
    ASSERT_EQ(0, ptrace_call(PTRACE_GETREGS, child, 0,
                             reinterpret_cast<unsigned long>(&saved)));

    const auto watched_address = reinterpret_cast<unsigned long>(&watched_word);
    ASSERT_EQ(0, ptrace_call(PTRACE_POKEUSER, child, kDr0Offset, watched_address));
    const unsigned long dr7 =
        kDrLocalEnable | ((kDrWrite | (kDrLen8 << 2)) << 16);
    ASSERT_EQ(0, ptrace_call(PTRACE_POKEUSER, child, kDr7Offset, dr7));

    user_regs_struct injected = saved;
    injected.rip = reinterpret_cast<unsigned long>(&coalesced_write_instruction);
    injected.rdi = watched_address;
    injected.eflags |= kEflagsTrap;
    ASSERT_EQ(0, ptrace_call(PTRACE_SETREGS, child, 0,
                             reinterpret_cast<unsigned long>(&injected)));
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));

    ASSERT_EQ(child, waitpid_deadline(child, &status));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGTRAP, WSTOPSIG(status));
    siginfo_t info = {};
    ASSERT_EQ(0, ptrace_call(PTRACE_GETSIGINFO, child, 0,
                             reinterpret_cast<unsigned long>(&info)));
    EXPECT_EQ(TRAP_TRACE, info.si_code);
    unsigned long dr6 = 0;
    ASSERT_EQ(0, peek_user(child, kDr6Offset, &dr6));
    EXPECT_EQ(kDr6Breakpoint0 | kDr6SingleStep,
              dr6 & (kDr6Breakpoint0 | kDr6SingleStep));

    ASSERT_EQ(0, ptrace_call(PTRACE_POKEUSER, child, kDr7Offset, 0));
    ASSERT_EQ(0, ptrace_call(PTRACE_SETREGS, child, 0,
                             reinterpret_cast<unsigned long>(&saved)));
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));
    ASSERT_EQ(child, waitpid_deadline(child, &status));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    child_guard.release();
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
