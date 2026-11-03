#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <linux/sched.h>
#include <sched.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

long CloneChildTid(pid_t* child_tid) {
    struct clone_args args = {};
    args.flags = CLONE_CHILD_SETTID;
    args.exit_signal = SIGCHLD;
    args.child_tid = reinterpret_cast<uintptr_t>(child_tid);
    return syscall(SYS_clone3, &args, sizeof(args));
}

void ExpectChildSuccess(pid_t child) {
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

#if defined(__x86_64__)
extern "C" {
extern const ptrdiff_t __rseq_offset __attribute__((weak));
extern const unsigned int __rseq_size __attribute__((weak));
}

struct alignas(32) RseqAbi {
    uint32_t cpu_id_start;
    uint32_t cpu_id;
    uint64_t rseq_cs;
    uint32_t flags;
    uint32_t node_id;
    uint32_t mm_cid;
    uint32_t padding;
};
constexpr uint32_t kRseqSignature = 0x53053053;

// Only call after fork: changing the test runner's libc registration would
// invalidate libc's own assumptions about its TLS region.
RseqAbi* RegisterPrivateRseq() {
    if (&__rseq_size != nullptr && &__rseq_offset != nullptr && __rseq_size != 0) {
        void* libc_rseq = static_cast<char*>(__builtin_thread_pointer()) + __rseq_offset;
        if (syscall(SYS_rseq, libc_rseq, sizeof(RseqAbi), 1, kRseqSignature) != 0) {
            return nullptr;
        }
    }
    void* page = mmap(nullptr, 4096, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED) return nullptr;
    auto* rseq = static_cast<RseqAbi*>(page);
    rseq->cpu_id_start = rseq->cpu_id = UINT32_MAX;
    if (syscall(SYS_rseq, rseq, sizeof(*rseq), 0, kRseqSignature) != 0) return nullptr;
    return rseq;
}
#endif

}  // namespace

TEST(UserReturnWork, ChildTidIsWrittenBeforeFirstUserInstructionWithCow) {
    auto* tid = static_cast<pid_t*>(mmap(nullptr, 4096, PROT_READ | PROT_WRITE,
                                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(tid, MAP_FAILED);
    constexpr pid_t kSentinel = -123;
    for (int iteration = 0; iteration < 16; ++iteration) {
        *tid = kSentinel;
        long child = CloneChildTid(tid);
        ASSERT_GE(child, 0) << strerror(errno);
        if (child == 0) {
            // Read before entering another syscall which could mask a late write.
            pid_t observed = *static_cast<volatile pid_t*>(tid);
            _exit(observed == syscall(SYS_gettid) ? 0 : 1);
        }
        ExpectChildSuccess(child);
        EXPECT_EQ(*tid, kSentinel);
    }
    EXPECT_EQ(munmap(tid, 4096), 0);
}

TEST(UserReturnWork, InvalidChildTidIsBestEffort) {
    long child = CloneChildTid(nullptr);
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) _exit(0);
    ExpectChildSuccess(child);
}

TEST(UserReturnWork, InaccessibleChildTidDoesNotKillChild) {
    void* page = mmap(nullptr, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(page, MAP_FAILED);
    long child = CloneChildTid(static_cast<pid_t*>(page));
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) _exit(0);
    ExpectChildSuccess(child);
    EXPECT_EQ(munmap(page, 4096), 0);
}

#if defined(__x86_64__)
TEST(UserReturnWork, RseqInheritedPrivatePageRemainsUsableAfterFork) {
    pid_t worker = fork();
    ASSERT_GE(worker, 0) << strerror(errno);
    if (worker == 0) {
        alarm(20);
        RseqAbi* rseq = RegisterPrivateRseq();
        if (rseq == nullptr) _exit(10);
        cpu_set_t allowed;
        CPU_ZERO(&allowed);
        if (sched_getaffinity(0, sizeof(allowed), &allowed) != 0) _exit(11);
        int cpus[2] = {-1, -1};
        for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
            if (CPU_ISSET(cpu, &allowed)) {
                if (cpus[0] < 0) cpus[0] = cpu;
                else { cpus[1] = cpu; break; }
            }
        }
        if (cpus[0] < 0) _exit(12);
        if (cpus[1] < 0) cpus[1] = cpus[0];
        for (int iteration = 0; iteration < 16; ++iteration) {
            const int cpu = cpus[iteration % 2];
            cpu_set_t pinned;
            CPU_ZERO(&pinned);
            CPU_SET(cpu, &pinned);
            if (sched_setaffinity(0, sizeof(pinned), &pinned) != 0) _exit(13);
            if (rseq->cpu_id != static_cast<uint32_t>(cpu)) _exit(14);
            pid_t child = fork();
            if (child < 0) _exit(15);
            if (child == 0) {
                const uint32_t start = *static_cast<volatile uint32_t*>(&rseq->cpu_id_start);
                const uint32_t current = *static_cast<volatile uint32_t*>(&rseq->cpu_id);
                _exit(start == static_cast<uint32_t>(cpu) && current == start ? 0 : 16);
            }
            int status = 0;
            if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
                WEXITSTATUS(status) != 0) _exit(17);
        }
        if (syscall(SYS_rseq, rseq, sizeof(*rseq), 1, kRseqSignature) != 0) _exit(18);
        _exit(0);
    }
    ExpectChildSuccess(worker);
}

TEST(UserReturnWork, MigrationNotificationSurvivesRealResumeHandler) {
    constexpr const char* path = "/sys/kernel/debug/sched/rseq_notify_selftest";
    if (access(path, R_OK) != 0 && errno == ENOENT) {
        GTEST_SKIP() << "requires a sched_selftest kernel";
    }
    pid_t worker = fork();
    ASSERT_GE(worker, 0) << strerror(errno);
    if (worker == 0) {
        alarm(20);
        RseqAbi* rseq = RegisterPrivateRseq();
        if (rseq == nullptr) _exit(20);
        int fd = open(path, O_RDONLY);
        if (fd < 0) _exit(21);
        char report[256] = {};
        ssize_t count = read(fd, report, sizeof(report) - 1);
        close(fd);
        if (count <= 0) _exit(22);
        // Check actual handler execution and notification preservation.
        if (strstr(report, "status=ok\n") == nullptr ||
            strstr(report, "migration_notification_injected=true\n") == nullptr ||
            strstr(report, "notification_preserved=true\n") == nullptr ||
            strstr(report, "handler_passes=2\n") == nullptr) {
            if (write(STDERR_FILENO, report, count) < 0) _exit(25);
            _exit(23);
        }
        if (syscall(SYS_rseq, rseq, sizeof(*rseq), 1, kRseqSignature) != 0) _exit(24);
        _exit(0);
    }
    ExpectChildSuccess(worker);
}
#endif

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
