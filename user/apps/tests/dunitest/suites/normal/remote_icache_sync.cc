#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <array>
#include <cerrno>
#include <cstdint>
#include <cstdio>
#include <cstring>

#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef PTRACE_SEIZE
#define PTRACE_SEIZE 0x4206
#endif
#ifndef PTRACE_INTERRUPT
#define PTRACE_INTERRUPT 0x4207
#endif

namespace {

constexpr size_t kInstructionBytes = sizeof(long);
constexpr int kDeadlineMs = 3000;

#if defined(__x86_64__)
constexpr std::array<uint8_t, kInstructionBytes> kInitial = {
    0xb8, 0x01, 0x00, 0x00, 0x00, 0xc3, 0x90, 0x90};
constexpr std::array<uint8_t, kInstructionBytes> kReplacement = {
    0xb8, 0x02, 0x00, 0x00, 0x00, 0xc3, 0x90, 0x90};
constexpr bool kHasInstructionFixture = true;
#elif defined(__riscv) && __riscv_xlen == 64
// li a0,1; ret / li a0,2; ret, encoded little-endian.
constexpr std::array<uint8_t, kInstructionBytes> kInitial = {
    0x13, 0x05, 0x10, 0x00, 0x67, 0x80, 0x00, 0x00};
constexpr std::array<uint8_t, kInstructionBytes> kReplacement = {
    0x13, 0x05, 0x20, 0x00, 0x67, 0x80, 0x00, 0x00};
constexpr bool kHasInstructionFixture = true;
#else
constexpr std::array<uint8_t, kInstructionBytes> kInitial = {};
constexpr std::array<uint8_t, kInstructionBytes> kReplacement = {};
constexpr bool kHasInstructionFixture = false;
#endif

static_assert(kInstructionBytes == 8);

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

int pin_current_to(int cpu) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    return sched_setaffinity(0, sizeof(set), &set);
}

void* map_code(size_t length) {
    void* mapping = mmap(nullptr, length, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (mapping == MAP_FAILED) return MAP_FAILED;
    memcpy(mapping, kInitial.data(), kInitial.size());
    __builtin___clear_cache(static_cast<char*>(mapping),
                            static_cast<char*>(mapping) + kInitial.size());
    if (mprotect(mapping, length, PROT_READ | PROT_EXEC) != 0) {
        const int error = errno;
        munmap(mapping, length);
        errno = error;
        return MAP_FAILED;
    }
    return mapping;
}

long ptrace_patch_word(pid_t child, void* code) {
    long replacement = 0;
    memcpy(&replacement, kReplacement.data(), kReplacement.size());
    return ptrace(PTRACE_POKEDATA, child, code, replacement);
}

int open_proc_mem(pid_t child) {
    char path[64] = {};
    const int length = snprintf(path, sizeof(path), "/proc/%d/mem", child);
    if (length <= 0 || static_cast<size_t>(length) >= sizeof(path)) {
        errno = EINVAL;
        return -1;
    }
    return open(path, O_RDWR | O_CLOEXEC);
}

TEST(RemoteIcacheSyncTest, InactiveChildSeesPatchedInstructionOnSwitchIn) {
    if (!kHasInstructionFixture) GTEST_SKIP() << "architecture has no fixture";
    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    void* code = map_code(static_cast<size_t>(page_size));
    ASSERT_NE(MAP_FAILED, code);

    int ready[2] = {};
    int resume[2] = {};
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(resume));
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(ready[0]);
        close(resume[1]);
        auto function = reinterpret_cast<int (*)()>(code);
        if (function() != 1) _exit(20);
        const char byte = 1;
        if (write(ready[1], &byte, 1) != 1) _exit(21);
        char command = 0;
        if (read(resume[0], &command, 1) != 1) _exit(22);
        _exit(function() == 2 ? 0 : 23);
    }
    ChildGuard child_guard(child);
    close(ready[1]);
    close(resume[0]);
    char byte = 0;
    ASSERT_TRUE(read_byte_deadline(ready[0], &byte));

    ASSERT_EQ(0, ptrace(PTRACE_ATTACH, child, nullptr, nullptr)) << strerror(errno);
    int status = 0;
    ASSERT_EQ(child, waitpid_deadline(child, &status));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(0, ptrace_patch_word(child, code)) << strerror(errno);
    ASSERT_EQ(0, ptrace(PTRACE_DETACH, child, nullptr, nullptr)) << strerror(errno);
    ASSERT_EQ(1, write(resume[1], &byte, 1));

    ASSERT_EQ(child, waitpid_deadline(child, &status));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    child_guard.release();
    close(ready[0]);
    close(resume[1]);
    EXPECT_EQ(0, munmap(code, static_cast<size_t>(page_size)));
}

struct ActiveState {
    int worker_ready;
    int replacement_published;
    int worker_done;
    int failures;
    int setup_errno;
    int worker_cpu;
    void* code;
};

void* active_worker(void* opaque) {
    auto* state = static_cast<ActiveState*>(opaque);
    if (pin_current_to(state->worker_cpu) != 0) {
        __atomic_store_n(&state->setup_errno, errno, __ATOMIC_RELEASE);
        __atomic_store_n(&state->worker_done, 1, __ATOMIC_RELEASE);
        return nullptr;
    }
    auto function = reinterpret_cast<int (*)()>(state->code);
    const int first = function();
    if (first != 1) __atomic_add_fetch(&state->failures, 1, __ATOMIC_RELAXED);
    __atomic_store_n(&state->worker_ready, 1, __ATOMIC_RELEASE);
    while (__atomic_load_n(&state->replacement_published, __ATOMIC_ACQUIRE) == 0) {
        const int value = function();
        if (value != 1 && value != 2) {
            __atomic_add_fetch(&state->failures, 1, __ATOMIC_RELAXED);
        }
    }
    for (int i = 0; i < 10000; ++i) {
        if (function() != 2) {
            __atomic_add_fetch(&state->failures, 1, __ATOMIC_RELAXED);
            break;
        }
    }
    __atomic_store_n(&state->worker_done, 1, __ATOMIC_RELEASE);
    return nullptr;
}

TEST(RemoteIcacheSyncTest, ActiveSiblingStopsSeeingOldInstructionAfterPatchReturns) {
    if (!kHasInstructionFixture) GTEST_SKIP() << "architecture has no fixture";
    ScopedAffinity affinity;
    ASSERT_TRUE(affinity.valid()) << strerror(errno);
    const int worker_cpu = first_cpu(affinity.saved());
    const int control_cpu = first_cpu(affinity.saved(), worker_cpu);
    if (worker_cpu < 0 || control_cpu < 0) {
        GTEST_SKIP() << "requires two available CPUs";
    }

    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    void* code = map_code(static_cast<size_t>(page_size));
    ASSERT_NE(MAP_FAILED, code);
    auto* state = static_cast<ActiveState*>(
        mmap(nullptr, sizeof(ActiveState), PROT_READ | PROT_WRITE,
             MAP_SHARED | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, state);
    memset(state, 0, sizeof(*state));
    state->worker_cpu = worker_cpu;
    state->code = code;

    int leader_ready[2] = {};
    int leader_resume[2] = {};
    ASSERT_EQ(0, pipe(leader_ready));
    ASSERT_EQ(0, pipe(leader_resume));
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(leader_ready[0]);
        close(leader_resume[1]);
        if (pin_current_to(control_cpu) != 0) _exit(30);
        pthread_t worker = {};
        if (pthread_create(&worker, nullptr, active_worker, state) != 0) _exit(31);
        const char byte = 1;
        if (write(leader_ready[1], &byte, 1) != 1) _exit(32);
        char command = 0;
        if (read(leader_resume[0], &command, 1) != 1) _exit(33);
        if (pthread_join(worker, nullptr) != 0) _exit(34);
        _exit(__atomic_load_n(&state->failures, __ATOMIC_ACQUIRE) == 0 &&
                      __atomic_load_n(&state->setup_errno, __ATOMIC_ACQUIRE) == 0
                  ? 0
                  : 35);
    }
    ChildGuard child_guard(child);
    close(leader_ready[1]);
    close(leader_resume[0]);
    ASSERT_TRUE(affinity.pin_to(control_cpu)) << strerror(errno);
    char byte = 0;
    ASSERT_TRUE(read_byte_deadline(leader_ready[0], &byte));
    ASSERT_TRUE(wait_shared_value(&state->worker_ready, 1));

    const int mem_fd = open_proc_mem(child);
    ASSERT_GE(mem_fd, 0) << strerror(errno);
    ASSERT_EQ(static_cast<ssize_t>(kReplacement.size()),
              pwrite(mem_fd, kReplacement.data(), kReplacement.size(),
                     static_cast<off_t>(reinterpret_cast<uintptr_t>(code))))
        << strerror(errno);
    ASSERT_EQ(0, close(mem_fd));
    __atomic_store_n(&state->replacement_published, 1, __ATOMIC_RELEASE);
    ASSERT_TRUE(wait_shared_value(&state->worker_done, 1));

    ASSERT_EQ(1, write(leader_resume[1], &byte, 1));
    int status = 0;
    ASSERT_EQ(child, waitpid_deadline(child, &status));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_EQ(0, __atomic_load_n(&state->failures, __ATOMIC_ACQUIRE));
    EXPECT_EQ(0, __atomic_load_n(&state->setup_errno, __ATOMIC_ACQUIRE));
    child_guard.release();
    close(leader_ready[0]);
    close(leader_resume[1]);
    EXPECT_EQ(0, munmap(state, sizeof(*state)));
    EXPECT_EQ(0, munmap(code, static_cast<size_t>(page_size)));
}

TEST(RemoteIcacheSyncTest, ProcMemWriteSynchronizesInstructionStream) {
    if (!kHasInstructionFixture) GTEST_SKIP() << "architecture has no fixture";
    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    void* code = map_code(static_cast<size_t>(page_size));
    ASSERT_NE(MAP_FAILED, code);
    int ready[2] = {};
    int resume[2] = {};
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(resume));

    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(ready[0]);
        close(resume[1]);
        auto function = reinterpret_cast<int (*)()>(code);
        if (function() != 1) _exit(40);
        const char byte = 1;
        if (write(ready[1], &byte, 1) != 1) _exit(41);
        char command = 0;
        if (read(resume[0], &command, 1) != 1) _exit(42);
        _exit(function() == 2 ? 0 : 43);
    }
    ChildGuard child_guard(child);
    close(ready[1]);
    close(resume[0]);
    char byte = 0;
    ASSERT_TRUE(read_byte_deadline(ready[0], &byte));
    const int mem_fd = open_proc_mem(child);
    ASSERT_GE(mem_fd, 0) << strerror(errno);
    ASSERT_EQ(static_cast<ssize_t>(kReplacement.size()),
              pwrite(mem_fd, kReplacement.data(), kReplacement.size(),
                     reinterpret_cast<uintptr_t>(code)))
        << strerror(errno);
    close(mem_fd);
    ASSERT_EQ(1, write(resume[1], &byte, 1));

    int status = 0;
    ASSERT_EQ(child, waitpid_deadline(child, &status));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    child_guard.release();
    close(ready[0]);
    close(resume[1]);
    EXPECT_EQ(0, munmap(code, static_cast<size_t>(page_size)));
}

TEST(RemoteIcacheSyncTest, PartialProcMemWriteStillSynchronizesCopiedInstruction) {
    if (!kHasInstructionFixture) GTEST_SKIP() << "architecture has no fixture";
    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    const size_t mapping_size = static_cast<size_t>(page_size) * 2;
    void* mapping = mmap(nullptr, mapping_size, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping);
    auto* code = static_cast<uint8_t*>(mapping) + page_size - kInstructionBytes;
    memcpy(code, kInitial.data(), kInitial.size());
    __builtin___clear_cache(reinterpret_cast<char*>(code),
                            reinterpret_cast<char*>(code + kInitial.size()));
    ASSERT_EQ(0, mprotect(mapping, static_cast<size_t>(page_size),
                          PROT_READ | PROT_EXEC));
    ASSERT_EQ(0, munmap(static_cast<uint8_t*>(mapping) + page_size,
                        static_cast<size_t>(page_size)));

    int ready[2] = {};
    int resume[2] = {};
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(resume));
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(ready[0]);
        close(resume[1]);
        auto function = reinterpret_cast<int (*)()>(code);
        if (function() != 1) _exit(50);
        const char byte = 1;
        if (write(ready[1], &byte, 1) != 1) _exit(51);
        char command = 0;
        if (read(resume[0], &command, 1) != 1) _exit(52);
        _exit(function() == 2 ? 0 : 53);
    }
    ChildGuard child_guard(child);
    close(ready[1]);
    close(resume[0]);
    char byte = 0;
    ASSERT_TRUE(read_byte_deadline(ready[0], &byte));
    const int mem_fd = open_proc_mem(child);
    ASSERT_GE(mem_fd, 0) << strerror(errno);
    std::array<uint8_t, kInstructionBytes * 2> patch = {};
    memcpy(patch.data(), kReplacement.data(), kReplacement.size());
    ASSERT_EQ(static_cast<ssize_t>(kInstructionBytes),
              pwrite(mem_fd, patch.data(), patch.size(),
                     reinterpret_cast<uintptr_t>(code)))
        << "partial /proc/pid/mem write errno=" << errno;
    close(mem_fd);
    ASSERT_EQ(1, write(resume[1], &byte, 1));

    int status = 0;
    ASSERT_EQ(child, waitpid_deadline(child, &status));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    child_guard.release();
    close(ready[0]);
    close(resume[1]);
    EXPECT_EQ(0, munmap(mapping, static_cast<size_t>(page_size)));
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
