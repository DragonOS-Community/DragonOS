#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <gtest/gtest.h>
#include <sched.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#include <time.h>
#include <cerrno>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <chrono>
#include <new>

namespace {
using Clock = std::chrono::steady_clock;
struct Shared {
    int child = 0, ready = 0, released = 0, clone_returned = 0;
    int hits = 0, premature_signal = 0;
    unsigned long long wall_ns = 0, cpu_ns = 0;
};
Shared* shared;
void OrdinarySignal(int) {
    if (!__atomic_load_n(&shared->released, __ATOMIC_ACQUIRE))
        __atomic_store_n(&shared->premature_signal, 1, __ATOMIC_RELEASE);
    __atomic_fetch_add(&shared->hits, 1, __ATOMIC_RELEASE);
}
unsigned long long Now(clockid_t id) {
    timespec value{};
    if (clock_gettime(id, &value)) _exit(90);
    return static_cast<unsigned long long>(value.tv_sec) * 1000000000ULL + value.tv_nsec;
}
struct ChildArgs {
    int gate;
    bool exec;
    char* const* argv;
};
// This runs with a dedicated clone stack. No allocation, stdio, pthread locks,
// or C++ destructors while sharing the suspended parent's address space.
int CloneChild(void* pointer) {
    auto* args = static_cast<ChildArgs*>(pointer);
    __atomic_store_n(&shared->child, static_cast<int>(syscall(SYS_getpid)), __ATOMIC_RELEASE);
    __atomic_store_n(&shared->ready, 1, __ATOMIC_RELEASE);
    char byte;
    long got;
    do { got = syscall(SYS_read, args->gate, &byte, 1); } while (got < 0 && errno == EINTR);
    if (got != 1) syscall(SYS_exit, 91);
    __atomic_store_n(&shared->released, 1, __ATOMIC_RELEASE);
    if (args->exec) {
        char* environment[] = {nullptr};
        syscall(SYS_execve, "/proc/self/exe", args->argv, environment);
        syscall(SYS_exit, 92);
    }
    syscall(SYS_exit, 0);
    __builtin_unreachable();
}

enum class Case { Exit, Exec, OrdinarySignal, ThreadKill, ProcessKill };

class KillableWait : public testing::Test {
 protected:
    pid_t worker_ = -1;
    int gate_[2] = {-1, -1}, finish_[2] = {-1, -1};
    int previous_subreaper_ = 0;
    bool subreaper_set_ = false;
    void* stack_ = MAP_FAILED;
    static constexpr size_t StackSize = 64 * 1024;

    void SetUp() override {
        ASSERT_EQ(prctl(PR_GET_CHILD_SUBREAPER, &previous_subreaper_, 0, 0, 0), 0);
        ASSERT_EQ(prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0), 0);
        subreaper_set_ = true;
        void* area = mmap(nullptr, sizeof(Shared), PROT_READ | PROT_WRITE,
                          MAP_SHARED | MAP_ANONYMOUS, -1, 0);
        ASSERT_NE(area, MAP_FAILED);
        shared = new (area) Shared{};
        stack_ = mmap(nullptr, StackSize, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        ASSERT_NE(stack_, MAP_FAILED);
        ASSERT_EQ(pipe(gate_), 0);
        ASSERT_EQ(pipe(finish_), 0);
    }
    // Exact owned children only. Probe waitpid before kill: a reaped/reused PID
    // must never be targeted. A dead worker reparents its vfork child to us.
    bool Reap(pid_t pid, int* status, bool terminate, bool allow_missing = false) {
        if (pid <= 0) return allow_missing;
        const auto deadline = Clock::now() + std::chrono::seconds(5);
        bool sent = false;
        while (Clock::now() < deadline) {
            const pid_t done = waitpid(pid, status, WNOHANG);
            if (done == pid) return true;
            if (done < 0) {
                if (errno == EINTR) continue;
                return allow_missing && errno == ECHILD;
            }
            if (terminate && !sent) { kill(pid, SIGKILL); sent = true; }
            usleep(1000);
        }
        return false;
    }
    void TearDown() override {
        // Release both gates even after a failed fatal-wake assertion. Otherwise
        // a broken killable wait could keep the worker alive during cleanup.
        // Our read ends remain open, so these bounded writes cannot SIGPIPE.
        for (int fd : {gate_[1], finish_[1]}) {
            if (fd >= 0) {
                ssize_t count;
                do { count = write(fd, "x", 1); } while (count < 0 && errno == EINTR);
                EXPECT_EQ(count, 1);
            }
        }
        int status = 0;
        EXPECT_TRUE(Reap(worker_, &status, true, true)) << "worker cleanup timed out";
        if (shared) {
            EXPECT_TRUE(Reap(__atomic_load_n(&shared->child, __ATOMIC_ACQUIRE), &status, true, true))
                << "vfork child cleanup timed out";
        }
        for (int fd : {gate_[0], gate_[1], finish_[0], finish_[1]}) {
            if (fd >= 0) { EXPECT_EQ(close(fd), 0); }
        }
        if (stack_ != MAP_FAILED) { EXPECT_EQ(munmap(stack_, StackSize), 0); }
        if (shared) { EXPECT_EQ(munmap(shared, sizeof(*shared)), 0); shared = nullptr; }
        if (subreaper_set_) {
            EXPECT_EQ(prctl(PR_SET_CHILD_SUBREAPER, previous_subreaper_, 0, 0, 0), 0);
        }
    }
    bool Await(int* value) {
        const auto deadline = Clock::now() + std::chrono::seconds(5);
        while (Clock::now() < deadline) {
            if (__atomic_load_n(value, __ATOMIC_ACQUIRE)) return true;
            usleep(1000);
        }
        return false;
    }
    void Exercise(Case which) {
        char finish_fd[24];
        snprintf(finish_fd, sizeof(finish_fd), "%d", finish_[0]);
        char helper[] = "--killable-exec-child";
        char self[] = "/proc/self/exe";
        char* argv[] = {self, helper, finish_fd, nullptr};
        ChildArgs args{gate_[0], which == Case::Exec, argv};
        worker_ = fork();
        ASSERT_GE(worker_, 0);
        if (worker_ == 0) {
            struct sigaction action{};
            action.sa_handler = OrdinarySignal;
            sigemptyset(&action.sa_mask);
            sigset_t mask; sigemptyset(&mask); sigaddset(&mask, SIGUSR1);
            if (sigaction(SIGUSR1, &action, nullptr) ||
                sigprocmask(SIG_UNBLOCK, &mask, nullptr)) _exit(93);
            const auto wall = Now(CLOCK_MONOTONIC), cpu = Now(CLOCK_THREAD_CPUTIME_ID);
            const pid_t child = clone(CloneChild, static_cast<char*>(stack_) + StackSize,
                                      CLONE_VM | CLONE_VFORK | SIGCHLD, &args);
            if (child < 0) _exit(94);
            shared->cpu_ns = Now(CLOCK_THREAD_CPUTIME_ID) - cpu;
            shared->wall_ns = Now(CLOCK_MONOTONIC) - wall;
            __atomic_store_n(&shared->clone_returned, 1, __ATOMIC_RELEASE);
            int status = 0;
            pid_t got;
            do { got = waitpid(child, &status, 0); } while (got < 0 && errno == EINTR);
            _exit(got == child && WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : 95);
        }
        ASSERT_TRUE(Await(&shared->ready));
        ASSERT_EQ(__atomic_load_n(&shared->clone_returned, __ATOMIC_ACQUIRE), 0);
        const pid_t child = __atomic_load_n(&shared->child, __ATOMIC_ACQUIRE);
        ASSERT_GT(child, 0);
        if (which == Case::ThreadKill || which == Case::ProcessKill) {
            const long sent = which == Case::ThreadKill
                ? syscall(SYS_tgkill, worker_, worker_, SIGKILL) : kill(worker_, SIGKILL);
            ASSERT_EQ(sent, 0);
            int status = 0;
            ASSERT_TRUE(Reap(worker_, &status, false));
            ASSERT_TRUE(WIFSIGNALED(status));
            ASSERT_EQ(WTERMSIG(status), SIGKILL);
            worker_ = -1;
            // The child remains alive at its gate while the killable parent
            // exits. No completion may access a freed waiter afterwards.
            ASSERT_EQ(__atomic_load_n(&shared->released, __ATOMIC_ACQUIRE), 0);
            ASSERT_EQ(write(gate_[1], "x", 1), 1);
            ASSERT_TRUE(Reap(child, &status, false));
            ASSERT_TRUE(WIFEXITED(status)); ASSERT_EQ(WEXITSTATUS(status), 0);
            return;
        }
        if (which == Case::OrdinarySignal) {
            ASSERT_EQ(syscall(SYS_tgkill, worker_, worker_, SIGUSR1), 0);
            // The gate, not elapsed time, keeps the child alive. This bounded
            // observation adds scheduling opportunities without a CPU threshold.
            const auto until = Clock::now() + std::chrono::milliseconds(50);
            while (Clock::now() < until) {
                ASSERT_EQ(__atomic_load_n(&shared->premature_signal, __ATOMIC_ACQUIRE), 0);
                usleep(1000);
            }
        }
        ASSERT_EQ(write(gate_[1], "x", 1), 1);
        ASSERT_TRUE(Await(&shared->clone_returned));
        if (which == Case::Exec) {
            // The exec helper cannot exit until this second gate opens. Thus
            // clone returning here proves release by exec, not by child exit.
            ASSERT_EQ(write(finish_[1], "x", 1), 1);
        }
        int status = 0;
        ASSERT_TRUE(Reap(worker_, &status, false));
        ASSERT_TRUE(WIFEXITED(status)); ASSERT_EQ(WEXITSTATUS(status), 0);
        worker_ = -1;
        EXPECT_EQ(__atomic_load_n(&shared->premature_signal, __ATOMIC_ACQUIRE), 0);
        EXPECT_EQ(__atomic_load_n(&shared->hits, __ATOMIC_ACQUIRE),
                  which == Case::OrdinarySignal ? 1 : 0);
        printf("killable_wait case=%d wall_ns=%llu parent_thread_cpu_ns=%llu\n",
               static_cast<int>(which), shared->wall_ns, shared->cpu_ns);
    }
};
TEST_F(KillableWait, ChildExitCompletesVfork) { Exercise(Case::Exit); }
TEST_F(KillableWait, ChildExecCompletesVforkBeforeExit) { Exercise(Case::Exec); }
TEST_F(KillableWait, OrdinarySignalWaitsForChildRelease) { Exercise(Case::OrdinarySignal); }
TEST_F(KillableWait, ThreadDirectedKillReleasesParent) { Exercise(Case::ThreadKill); }
TEST_F(KillableWait, ProcessDirectedKillReleasesParent) { Exercise(Case::ProcessKill); }
}  // namespace

int main(int argc, char** argv) {
    if (argc == 3 && !strcmp(argv[1], "--killable-exec-child")) {
        char* end = nullptr;
        const long fd = strtol(argv[2], &end, 10);
        if (!end || *end || fd < 0 || fd > 0x7fffffff) return 96;
        char byte; ssize_t count;
        do { count = read(static_cast<int>(fd), &byte, 1); } while (count < 0 && errno == EINTR);
        return count == 1 ? 0 : 97;
    }
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
