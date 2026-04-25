#include <gtest/gtest.h>

#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <string.h>
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
