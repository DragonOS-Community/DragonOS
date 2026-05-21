#include <gtest/gtest.h>

#include <atomic>
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

int FirstCpu(const cpu_set_t& set) {
    for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
        if (CPU_ISSET(cpu, &set)) {
            return cpu;
        }
    }
    return -1;
}

int NextCpu(const cpu_set_t& set, int after) {
    for (int cpu = after + 1; cpu < CPU_SETSIZE; ++cpu) {
        if (CPU_ISSET(cpu, &set)) {
            return cpu;
        }
    }
    return -1;
}

std::string CpuSetToString(const cpu_set_t& set) {
    std::string out = "{";
    bool first = true;
    for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
        if (!CPU_ISSET(cpu, &set)) {
            continue;
        }
        if (!first) {
            out += ",";
        }
        first = false;
        out += std::to_string(cpu);
    }
    out += "}";
    return out;
}

void ExpectCpuSetEq(const cpu_set_t& expected, const cpu_set_t& actual) {
    for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
        EXPECT_EQ(CPU_ISSET(cpu, &expected), CPU_ISSET(cpu, &actual))
            << "cpu=" << cpu << " expected=" << CpuSetToString(expected)
            << " actual=" << CpuSetToString(actual);
    }
}

int SetOneCpu(pid_t tid, int cpu) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    return sched_setaffinity(tid, sizeof(set), &set);
}

int CurrentCpu() {
    unsigned int cpu = 0;
    if (syscall(SYS_getcpu, &cpu, nullptr, nullptr) != 0) {
        return -1;
    }
    return static_cast<int>(cpu);
}

bool WaitUntil(const std::atomic<int>& value, int expected, int rounds) {
    for (int i = 0; i < rounds; ++i) {
        if (value.load(std::memory_order_acquire) == expected) {
            return true;
        }
        sched_yield();
    }
    return false;
}

class SchedAffinityFixture : public ::testing::Test {
protected:
    void SetUp() override {
        CPU_ZERO(&original_);
        ASSERT_EQ(0, sched_getaffinity(0, sizeof(original_), &original_))
            << "sched_getaffinity failed: errno=" << errno << " (" << strerror(errno) << ")";
        ASSERT_GE(FirstCpu(original_), 0) << "initial affinity is empty";
    }

    void TearDown() override {
        if (sched_setaffinity(0, sizeof(original_), &original_) != 0) {
            ADD_FAILURE() << "restore sched_setaffinity failed: errno=" << errno << " ("
                          << strerror(errno) << ")";
        }
    }

    cpu_set_t SingleCpuMask(int cpu) const {
        cpu_set_t set;
        CPU_ZERO(&set);
        CPU_SET(cpu, &set);
        return set;
    }

    cpu_set_t original_ {};
};

struct BusyWorkerState {
    std::atomic<int> ready {0};
    std::atomic<int> done {0};
    std::atomic<int> seen_dst {0};
    std::atomic<int> tid {0};
    std::atomic<int> last_cpu {-1};
    std::atomic<int> setup_errno {0};
    int src_cpu = -1;
    int dst_cpu = -1;
};

void* BusyWorker(void* arg) {
    auto* state = static_cast<BusyWorkerState*>(arg);
    state->tid.store(static_cast<int>(syscall(SYS_gettid)), std::memory_order_release);

    if (SetOneCpu(0, state->src_cpu) != 0) {
        state->setup_errno.store(errno, std::memory_order_release);
        state->done.store(1, std::memory_order_release);
        return reinterpret_cast<void*>(1);
    }

    state->ready.store(1, std::memory_order_release);
    while (state->done.load(std::memory_order_acquire) == 0) {
        const int cpu = CurrentCpu();
        state->last_cpu.store(cpu, std::memory_order_release);
        if (cpu == state->dst_cpu) {
            state->seen_dst.store(1, std::memory_order_release);
        }
    }

    return nullptr;
}

}  // namespace

TEST_F(SchedAffinityFixture, SetGetRoundTripSingleCpu) {
    const int cpu = FirstCpu(original_);
    ASSERT_GE(cpu, 0);

    cpu_set_t target = SingleCpuMask(cpu);
    ASSERT_EQ(0, sched_setaffinity(0, sizeof(target), &target))
        << "sched_setaffinity failed: errno=" << errno << " (" << strerror(errno) << ")";

    cpu_set_t current;
    CPU_ZERO(&current);
    ASSERT_EQ(0, sched_getaffinity(0, sizeof(current), &current))
        << "sched_getaffinity failed: errno=" << errno << " (" << strerror(errno) << ")";

    ExpectCpuSetEq(target, current);
}

TEST_F(SchedAffinityFixture, ForkChildInheritsAffinity) {
    const int cpu = FirstCpu(original_);
    ASSERT_GE(cpu, 0);

    cpu_set_t target = SingleCpuMask(cpu);
    ASSERT_EQ(0, sched_setaffinity(0, sizeof(target), &target))
        << "sched_setaffinity failed: errno=" << errno << " (" << strerror(errno) << ")";

    pid_t pid = fork();
    ASSERT_GE(pid, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (pid == 0) {
        cpu_set_t child;
        CPU_ZERO(&child);
        if (sched_getaffinity(0, sizeof(child), &child) != 0) {
            _exit(10);
        }

        for (int i = 0; i < CPU_SETSIZE; ++i) {
            if (CPU_ISSET(i, &child) != CPU_ISSET(i, &target)) {
                _exit(11);
            }
        }
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(pid, waitpid(pid, &status, 0))
        << "waitpid failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_TRUE(WIFEXITED(status)) << "child did not exit normally, status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status)) << "child exit status=" << WEXITSTATUS(status);
}

TEST_F(SchedAffinityFixture, EmptyMaskRejected) {
    cpu_set_t empty;
    CPU_ZERO(&empty);

    errno = 0;
    EXPECT_EQ(-1, sched_setaffinity(0, sizeof(empty), &empty));
    EXPECT_EQ(EINVAL, errno);
}

TEST_F(SchedAffinityFixture, ChildCanUpdateOwnAffinityRoundTrip) {
    const int cpu0 = FirstCpu(original_);
    ASSERT_GE(cpu0, 0);
    const int cpu1 = NextCpu(original_, cpu0);
    const int target_cpu = (cpu1 >= 0) ? cpu1 : cpu0;

    pid_t pid = fork();
    ASSERT_GE(pid, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (pid == 0) {
        cpu_set_t target;
        cpu_set_t current;
        CPU_ZERO(&target);
        CPU_ZERO(&current);
        CPU_SET(target_cpu, &target);

        if (sched_setaffinity(0, sizeof(target), &target) != 0) {
            _exit(20);
        }
        if (sched_getaffinity(0, sizeof(current), &current) != 0) {
            _exit(21);
        }

        for (int i = 0; i < CPU_SETSIZE; ++i) {
            if (CPU_ISSET(i, &current) != CPU_ISSET(i, &target)) {
                _exit(22);
            }
        }
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(pid, waitpid(pid, &status, 0))
        << "waitpid failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_TRUE(WIFEXITED(status)) << "child did not exit normally, status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status)) << "child exit status=" << WEXITSTATUS(status);
}

TEST_F(SchedAffinityFixture, CurrentRunningThreadCanBeMigratedByAnotherThread) {
    const int src_cpu = FirstCpu(original_);
    ASSERT_GE(src_cpu, 0);
    const int dst_cpu = NextCpu(original_, src_cpu);
    if (dst_cpu < 0) {
        GTEST_SKIP() << "requires at least two allowed CPUs";
    }

    BusyWorkerState state;
    state.src_cpu = src_cpu;
    state.dst_cpu = dst_cpu;

    pthread_t thread {};
    ASSERT_EQ(0, pthread_create(&thread, nullptr, BusyWorker, &state));

    if (!WaitUntil(state.ready, 1, 100000)) {
        state.done.store(1, std::memory_order_release);
        void* result = nullptr;
        ASSERT_EQ(0, pthread_join(thread, &result));
        FAIL() << "worker did not become ready, setup errno="
               << state.setup_errno.load(std::memory_order_acquire);
    }
    const int worker_tid = state.tid.load(std::memory_order_acquire);
    ASSERT_GT(worker_tid, 0);

    ASSERT_EQ(0, SetOneCpu(worker_tid, dst_cpu))
        << "sched_setaffinity(worker) failed: errno=" << errno << " (" << strerror(errno)
        << ")";
    EXPECT_TRUE(WaitUntil(state.seen_dst, 1, 200000))
        << "worker did not reach destination CPU, last cpu="
        << state.last_cpu.load(std::memory_order_acquire);

    state.done.store(1, std::memory_order_release);
    void* result = nullptr;
    ASSERT_EQ(0, pthread_join(thread, &result));
    EXPECT_EQ(nullptr, result);
}

TEST_F(SchedAffinityFixture, CurrentThreadCanMigrateItself) {
    const int src_cpu = FirstCpu(original_);
    ASSERT_GE(src_cpu, 0);
    const int dst_cpu = NextCpu(original_, src_cpu);
    if (dst_cpu < 0) {
        GTEST_SKIP() << "requires at least two allowed CPUs";
    }

    ASSERT_EQ(0, SetOneCpu(0, src_cpu))
        << "sched_setaffinity(src) failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_EQ(0, SetOneCpu(0, dst_cpu))
        << "sched_setaffinity(dst) failed: errno=" << errno << " (" << strerror(errno) << ")";

    bool reached_dst = false;
    for (int i = 0; i < 200000; ++i) {
        if (CurrentCpu() == dst_cpu) {
            reached_dst = true;
            break;
        }
        sched_yield();
    }

    EXPECT_TRUE(reached_dst) << "self migration did not reach destination CPU";
}

TEST_F(SchedAffinityFixture, SameUidParentCanUpdateChildAffinity) {
    int ready_pipe[2];
    ASSERT_EQ(0, pipe(ready_pipe)) << "pipe failed: errno=" << errno << " (" << strerror(errno)
                                   << ")";

    pid_t helper = fork();
    ASSERT_GE(helper, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (helper == 0) {
        close(ready_pipe[0]);
        close(ready_pipe[1]);

        const uid_t test_uid = (geteuid() == 0) ? static_cast<uid_t>(1000) : geteuid();
        if (setresuid(test_uid, test_uid, test_uid) != 0) {
            _exit(30);
        }

        cpu_set_t available;
        CPU_ZERO(&available);
        if (sched_getaffinity(0, sizeof(available), &available) != 0) {
            _exit(31);
        }

        const int cpu = FirstCpu(available);
        if (cpu < 0) {
            _exit(32);
        }

        cpu_set_t target;
        CPU_ZERO(&target);
        CPU_SET(cpu, &target);

        int sync_pipe[2];
        if (pipe(sync_pipe) != 0) {
            _exit(33);
        }

        pid_t child = fork();
        if (child < 0) {
            _exit(34);
        }

        if (child == 0) {
            close(sync_pipe[1]);
            char token = 0;
            if (read(sync_pipe[0], &token, sizeof(token)) != sizeof(token)) {
                _exit(40);
            }
            close(sync_pipe[0]);

            cpu_set_t current;
            CPU_ZERO(&current);
            if (sched_getaffinity(0, sizeof(current), &current) != 0) {
                _exit(41);
            }

            for (int i = 0; i < CPU_SETSIZE; ++i) {
                if (CPU_ISSET(i, &current) != CPU_ISSET(i, &target)) {
                    _exit(42);
                }
            }
            _exit(0);
        }

        close(sync_pipe[0]);
        if (sched_setaffinity(child, sizeof(target), &target) != 0) {
            _exit(35);
        }

        const char token = 'x';
        if (write(sync_pipe[1], &token, sizeof(token)) != sizeof(token)) {
            _exit(36);
        }
        close(sync_pipe[1]);

        int child_status = 0;
        if (waitpid(child, &child_status, 0) != child) {
            _exit(37);
        }
        if (!WIFEXITED(child_status)) {
            _exit(38);
        }
        _exit(WEXITSTATUS(child_status));
    }

    close(ready_pipe[0]);
    close(ready_pipe[1]);

    int status = 0;
    ASSERT_EQ(helper, waitpid(helper, &status, 0))
        << "waitpid failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_TRUE(WIFEXITED(status)) << "helper did not exit normally, status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status)) << "helper exit status=" << WEXITSTATUS(status);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
