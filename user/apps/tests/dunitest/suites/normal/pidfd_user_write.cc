#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <atomic>

namespace {

size_t PageSize() { return static_cast<size_t>(sysconf(_SC_PAGESIZE)); }

class Mapping {
 public:
    explicit Mapping(size_t bytes) : bytes_(bytes) {
        value_ = mmap(nullptr, bytes, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    }
    ~Mapping() { if (value_ != MAP_FAILED) munmap(value_, bytes_); }
    void* get() const { return value_; }
 private:
    void* value_;
    size_t bytes_;
};

enum class ReapResult { Reaped, Deadline, Lost, Error };

ReapResult ReapWithin(pid_t child, int* status) {
    timespec now{};
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return ReapResult::Error;
    const int64_t deadline = now.tv_sec * INT64_C(1000000000) + now.tv_nsec + INT64_C(2000000000);
    for (;;) {
        const pid_t result = waitpid(child, status, WNOHANG);
        if (result == child) return ReapResult::Reaped;
        if (result < 0 && errno != EINTR) return errno == ECHILD ? ReapResult::Lost : ReapResult::Error;
        if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return ReapResult::Error;
        if (now.tv_sec * INT64_C(1000000000) + now.tv_nsec >= deadline) return ReapResult::Deadline;
        const timespec pause{0, 1000000};
        nanosleep(&pause, nullptr);
    }
}

void FinishChild(pid_t child) {
    int status = 0;
    ReapResult result = ReapWithin(child, &status);
    if (result == ReapResult::Deadline || result == ReapResult::Error) {
        kill(child, SIGKILL);
        result = ReapWithin(child, &status);
    }
    ASSERT_EQ(result, ReapResult::Reaped) << "child did not terminate within its absolute watchdog";
    EXPECT_TRUE(WIFEXITED(status)) << status;
    if (WIFEXITED(status)) { EXPECT_EQ(WEXITSTATUS(status), 0); }
}

pid_t ForkWithPidfd(void* output, unsigned long extra = 0) {
    const pid_t child = static_cast<pid_t>(syscall(SYS_clone,
        static_cast<unsigned long>(CLONE_PIDFD | SIGCHLD) | extra,
        nullptr, output, nullptr, nullptr));
    if (child == 0) _exit(0);
    return child;
}

void CheckPidfd(void* output, pid_t child) {
    int fd = -1;
    memcpy(&fd, output, sizeof(fd));
    EXPECT_GE(fd, 0);
    if (fd >= 0) { EXPECT_EQ(close(fd), 0); }
    FinishChild(child);
}

int ExitClone(void*) { _exit(0); }

struct SyncContext {
    int fd;
    std::atomic<bool> stop{false};
    std::atomic<unsigned> cycles{0};
    std::atomic<int> error{0};
};

void* ContinuousSync(void* opaque) {
    auto* context = static_cast<SyncContext*>(opaque);
    while (!context->stop.load()) {
        if (fsync(context->fd) != 0) {
            context->error.store(errno);
            break;
        }
        context->cycles.fetch_add(1);
    }
    return nullptr;
}

// Run the potentially competing syscall only in a supervised process group.
// If a kernel regression stalls fsync/clone/join, the root test process still
// has an absolute watchdog and can kill the whole still-owned group.
int SharedWritebackWorker(int ready_fd, bool forever) {
    if (setpgid(0, 0) != 0) return 20;
    char path[] = "/tmp/dunitest_pidfd_shared_write_XXXXXX";
    const int fd = mkstemp(path);
    if (fd < 0) return 21;
    unlink(path);
    if (ftruncate(fd, PageSize()) != 0) return 22;
    void* output = mmap(nullptr, PageSize(), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (output == MAP_FAILED) return 23;
    int sentinel = -12345;
    memcpy(output, &sentinel, sizeof(sentinel));
    SyncContext context{fd};
    pthread_t thread;
    if (pthread_create(&thread, nullptr, ContinuousSync, &context) != 0) return 24;
    timespec start{}, now{};
    if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) _exit(25);
    while (context.cycles.load() == 0 && context.error.load() == 0) {
        if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) _exit(25);
        if ((now.tv_sec - start.tv_sec) * INT64_C(1000000000) + now.tv_nsec - start.tv_nsec >
            INT64_C(1000000000)) _exit(26);
        const timespec pause{0, 1000000};
        nanosleep(&pause, nullptr);
    }
    if (context.error.load() != 0) _exit(27);
    if (ready_fd >= 0) {
        const char ready = 'R';
        if (write(ready_fd, &ready, 1) != 1) _exit(28);
        close(ready_fd);
    }
    for (unsigned attempt = 0; forever || attempt < 16; ++attempt) {
        const pid_t child = ForkWithPidfd(output);
        if (child <= 0) _exit(29);
        int pidfd = -1;
        memcpy(&pidfd, output, sizeof(pidfd));
        if (pidfd < 0 || close(pidfd) != 0) _exit(30);
        int status = 0;
        if (ReapWithin(child, &status) != ReapResult::Reaped || !WIFEXITED(status) || WEXITSTATUS(status) != 0)
            _exit(31);
    }
    context.stop.store(true);
    if (pthread_join(thread, nullptr) != 0) _exit(32);
    if (context.cycles.load() == 0 || context.error.load() != 0) return 33;
    if (munmap(output, PageSize()) != 0 || close(fd) != 0) return 34;
    return 0;
}

void FinishWorkerGroup(pid_t child) {
    int status = 0;
    timespec now{};
    ReapResult observed = ReapResult::Error;
    if (clock_gettime(CLOCK_MONOTONIC, &now) == 0) {
        const int64_t deadline = now.tv_sec * INT64_C(1000000000) + now.tv_nsec + INT64_C(2000000000);
        for (;;) {
            siginfo_t info{};
            const int waited = waitid(P_PID, child, &info, WEXITED | WNOHANG | WNOWAIT);
            if (waited != 0 && errno != EINTR) {
                observed = errno == ECHILD ? ReapResult::Lost : ReapResult::Error;
                break;
            }
            if (waited == 0 && info.si_pid == child) {
                observed = ReapResult::Reaped;  // observed, deliberately NOT reaped
                break;
            }
            if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) break;
            if (now.tv_sec * INT64_C(1000000000) + now.tv_nsec >= deadline) {
                observed = ReapResult::Deadline;
                break;
            }
            const timespec pause{0, 1000000};
            nanosleep(&pause, nullptr);
        }
    }
    if (observed != ReapResult::Lost) {
        // Even a worker-stage failure can leave a descendant. Its unreaped
        // leader pins the PGID while all still-owned descendants are stopped.
        kill(-child, SIGKILL);
        kill(child, SIGKILL);
    }
    ASSERT_NE(observed, ReapResult::Lost);
    const ReapResult result = ReapWithin(child, &status);
    ASSERT_EQ(result, ReapResult::Reaped);
    ASSERT_TRUE(WIFEXITED(status)) << status;
    EXPECT_EQ(WEXITSTATUS(status), 0) << "writeback worker stage";
}

}  // namespace

TEST(PidfdUserWrite, CrossPageOutputResolvesBothPagesBeforeCopy) {
    Mapping memory(2 * PageSize());
    ASSERT_NE(memory.get(), MAP_FAILED);
    auto* output = static_cast<unsigned char*>(memory.get()) + PageSize() - 2;
    const int sentinel = -12345;
    memcpy(output, &sentinel, sizeof(sentinel));
    const pid_t child = ForkWithPidfd(output);
    ASSERT_GT(child, 0) << strerror(errno);
    CheckPidfd(output, child);
}

TEST(PidfdUserWrite, PrivateOutputCowDoesNotModifyExistingChild) {
    Mapping memory(PageSize());
    ASSERT_NE(memory.get(), MAP_FAILED);
    const int sentinel = -12345;
    memcpy(memory.get(), &sentinel, sizeof(sentinel));
    int gate[2];
    ASSERT_EQ(pipe(gate), 0);
    pid_t observer = fork();
    ASSERT_GE(observer, 0) << strerror(errno);
    if (observer == 0) {
        close(gate[1]);
        char byte = 0;
        const ssize_t count = read(gate[0], &byte, 1);
        int observed = 0;
        memcpy(&observed, memory.get(), sizeof(observed));
        _exit(count == 1 && observed == sentinel ? 0 : 1);
    }
    close(gate[0]);
    const pid_t child = ForkWithPidfd(memory.get());
    EXPECT_GT(child, 0) << strerror(errno);
    if (child > 0) CheckPidfd(memory.get(), child);
    const char release = 'R';
    EXPECT_EQ(write(gate[1], &release, 1), 1);
    close(gate[1]);
    FinishChild(observer);
}

TEST(PidfdUserWrite, BadSecondPageDoesNotWriteFirstPagePrefix) {
    Mapping memory(2 * PageSize());
    ASSERT_NE(memory.get(), MAP_FAILED);
    auto* output = static_cast<unsigned char*>(memory.get()) + PageSize() - 2;
    output[0] = 0x5a;
    output[1] = 0xa5;
    ASSERT_EQ(mprotect(static_cast<unsigned char*>(memory.get()) + PageSize(), PageSize(), PROT_NONE), 0);
    errno = 0;
    const pid_t child = ForkWithPidfd(output);
    const int error = errno;
    if (child > 0) FinishChild(child);
    EXPECT_EQ(child, -1);
    EXPECT_EQ(error, EFAULT);
    EXPECT_EQ(output[0], 0x5a);
    EXPECT_EQ(output[1], 0xa5);
}

TEST(PidfdUserWrite, ReadySharedMmOutputDoesNotInventMinorFault) {
    Mapping output(PageSize());
    Mapping stack(4 * PageSize());
    ASSERT_NE(output.get(), MAP_FAILED);
    ASSERT_NE(stack.get(), MAP_FAILED);
    memset(output.get(), 0, PageSize());
    memset(stack.get(), 0, 4 * PageSize());
    rusage before{}, after{};
    ASSERT_EQ(getrusage(RUSAGE_SELF, &after), 0);  // warm syscall/output storage
    ASSERT_EQ(getrusage(RUSAGE_SELF, &before), 0);
    const pid_t child = clone(ExitClone, static_cast<unsigned char*>(stack.get()) + 4 * PageSize(),
                             CLONE_VM | CLONE_PIDFD | SIGCHLD, nullptr, output.get());
    const int error = errno;
    const int usage_result = getrusage(RUSAGE_SELF, &after);
    if (usage_result != 0 && child > 0) FinishChild(child);
    ASSERT_EQ(usage_result, 0);
    ASSERT_GT(child, 0) << strerror(error);
    EXPECT_EQ(after.ru_minflt, before.ru_minflt);
    EXPECT_EQ(after.ru_majflt, before.ru_majflt);
    CheckPidfd(output.get(), child);
}

TEST(PidfdUserWrite, NamespaceFailurePrecedesBadOutputAndKeepsSentinel) {
    const pid_t observer = fork();
    ASSERT_GE(observer, 0) << strerror(errno);
    if (observer == 0) {
        if (unshare(CLONE_NEWPID) != 0) _exit(10);
        Mapping output(PageSize());
        if (output.get() == MAP_FAILED) _exit(11);
        const int sentinel = -12345;
        memcpy(output.get(), &sentinel, sizeof(sentinel));
        if (mprotect(output.get(), PageSize(), PROT_READ) != 0) _exit(12);
        // active PID namespace != pid_ns_for_children: copy_pid_ns rejects
        // this before PID allocation and Linux's subsequent put_user.
        errno = 0;
        const pid_t child = ForkWithPidfd(output.get(), CLONE_NEWPID);
        const int error = errno;
        if (child > 0) { kill(child, SIGKILL); int status; ReapWithin(child, &status); }
        int observed = 0;
        memcpy(&observed, output.get(), sizeof(observed));
        _exit(child == -1 && error == EINVAL && observed == sentinel ? 0 : 13);
    }
    FinishChild(observer);
}

TEST(PidfdUserWrite, SharedFileWritebackRefaultsWithoutSpuriousEfault) {
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) _exit(SharedWritebackWorker(-1, false));
    setpgid(child, child);
    FinishWorkerGroup(child);
}

TEST(PidfdUserWrite, FatalSignalEndsConcurrentWritebackRetry) {
    int ready[2];
    ASSERT_EQ(pipe(ready), 0);
    const pid_t child = fork();
    if (child < 0) {
        close(ready[0]);
        close(ready[1]);
        FAIL() << strerror(errno);
    }
    if (child == 0) {
        close(ready[0]);
        _exit(SharedWritebackWorker(ready[1], true));
    }
    setpgid(child, child);
    close(ready[1]);
    pollfd source{ready[0], POLLIN, 0};
    // One bounded observation of an actual running fsync worker. The root
    // runner never enters its competing clone/fsync calls itself.
    const int result = poll(&source, 1, 1000);
    char marker = 0;
    const bool running = result == 1 && (source.revents & POLLIN) != 0 && read(ready[0], &marker, 1) == 1 && marker == 'R';
    close(ready[0]);
    const timespec pause{0, 20000000};
    if (running) nanosleep(&pause, nullptr);
    kill(-child, SIGKILL);
    kill(child, SIGKILL);
    int status = 0;
    const ReapResult reaped = ReapWithin(child, &status);
    EXPECT_TRUE(running);
    ASSERT_EQ(reaped, ReapResult::Reaped);
    ASSERT_TRUE(WIFSIGNALED(status)) << status;
    EXPECT_EQ(WTERMSIG(status), SIGKILL);
}

TEST(PidfdUserWrite, RepeatedLateBadOutputLeavesParentFdSlotsReusable) {
    Mapping output(PageSize());
    ASSERT_NE(output.get(), MAP_FAILED);
    const int sentinel = -12345;
    memcpy(output.get(), &sentinel, sizeof(sentinel));
    ASSERT_EQ(mprotect(output.get(), PageSize(), PROT_READ), 0);
    for (unsigned attempt = 0; attempt < 16; ++attempt) {
        errno = 0;
        // Include the parent-only SEMUNDO preparation before late put_user;
        // no child undo owner may have been installed when this fails.
        const pid_t child = ForkWithPidfd(output.get(), CLONE_SYSVSEM);
        const int error = errno;
        if (child > 0) FinishChild(child);
        ASSERT_EQ(child, -1);
        EXPECT_EQ(error, EFAULT);
        int observed = 0;
        memcpy(&observed, output.get(), sizeof(observed));
        EXPECT_EQ(observed, sentinel);
    }
    ASSERT_EQ(mprotect(output.get(), PageSize(), PROT_READ | PROT_WRITE), 0);
    const pid_t child = ForkWithPidfd(output.get(), CLONE_SYSVSEM);
    ASSERT_GT(child, 0) << strerror(errno);
    CheckPidfd(output.get(), child);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
