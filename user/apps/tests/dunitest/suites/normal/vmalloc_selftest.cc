#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <string>
#include <thread>
#include <vector>

namespace {

constexpr const char* kSelftestPath = "/sys/kernel/debug/mm/vmalloc_selftest";
constexpr std::array<const char*, 8> kChecks = {
    "zero_alignment_cross_page", "partial_failure_rollback", "noncontiguous_backing",
    "shared_table_lifetime", "layout_boundaries", "accounting", "smp_visibility_reuse",
    "completion_deadline",
};

std::string ReadFd(int fd, size_t chunk_size) {
    std::string report;
    char buffer[256];
    while (true) {
        const ssize_t count = read(fd, buffer, chunk_size);
        if (count == 0) {
            return report;
        }
        if (count < 0) {
            if (errno == EINTR) {
                continue;
            }
            return {};
        }
        report.append(buffer, static_cast<size_t>(count));
    }
}

// No GoogleTest assertions here: forked children report failures by exit code.
std::string ReadReport() {
    const int fd = open(kSelftestPath, O_RDONLY);
    if (fd < 0) {
        return {};
    }
    auto report = ReadFd(fd, 256);
    if (close(fd) < 0) {
        return {};
    }
    return report;
}

bool ReportOk(const std::string& report) {
    if (report.find("status=ok\n") == std::string::npos ||
        report.find("summary_fail=0\n") == std::string::npos) {
        return false;
    }
    for (const char* name : kChecks) {
        if (report.find(std::string(name) + "=ok\n") == std::string::npos) {
            return false;
        }
    }
    return true;
}

std::vector<int> AllowedCpus() {
    cpu_set_t allowed;
    CPU_ZERO(&allowed);
    if (sched_getaffinity(0, sizeof(allowed), &allowed) < 0) {
        return {};
    }
    std::vector<int> cpus;
    for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
        if (CPU_ISSET(cpu, &allowed)) {
            cpus.push_back(cpu);
        }
    }
    return cpus;
}

bool BindCpu(int cpu) {
    cpu_set_t affinity;
    CPU_ZERO(&affinity);
    CPU_SET(cpu, &affinity);
    return sched_setaffinity(0, sizeof(affinity), &affinity) == 0 && sched_getcpu() == cpu;
}

}  // namespace

TEST(VmallocSelftest, ChecksPassAndSnapshotIsStableAcrossPartialReads) {
    const int fd = open(kSelftestPath, O_RDONLY);
    ASSERT_GE(fd, 0) << strerror(errno);
    const auto report = ReadFd(fd, 7);
    EXPECT_EQ(lseek(fd, 0, SEEK_SET), 0) << strerror(errno);
    const auto reread = ReadFd(fd, 256);
    EXPECT_EQ(close(fd), 0) << strerror(errno);
    EXPECT_TRUE(ReportOk(report)) << report;
    EXPECT_EQ(report, reread);
    EXPECT_EQ(report, ReadReport());
}

TEST(VmallocSelftest, ConcurrentThreadsRunOnAllowedCpus) {
    const auto cpus = AllowedCpus();
    ASSERT_FALSE(cpus.empty()) << strerror(errno);
    constexpr size_t kWorkers = 4;
    std::array<std::thread, kWorkers> workers;
    std::array<bool, kWorkers> bound{};
    std::array<std::string, kWorkers> first;
    std::array<std::string, kWorkers> second;
    for (size_t index = 0; index < kWorkers; ++index) {
        workers[index] = std::thread([&, index] {
            bound[index] = BindCpu(cpus[index % cpus.size()]);
            if (bound[index]) {
                first[index] = ReadReport();
                second[index] = ReadReport();
            }
        });
    }
    for (auto& worker : workers) {
        worker.join();
    }
    for (size_t index = 0; index < kWorkers; ++index) {
        EXPECT_TRUE(bound[index]) << index;
        EXPECT_TRUE(ReportOk(first[index])) << first[index];
        EXPECT_TRUE(ReportOk(second[index])) << second[index];
        EXPECT_EQ(first[index], second[index]);
    }
}

TEST(VmallocSelftest, ForkedAddressSpacesCanAllocateAndReuseSharedKernelMappings) {
    const auto cpus = AllowedCpus();
    ASSERT_FALSE(cpus.empty()) << strerror(errno);
    int gate[2];
    ASSERT_EQ(pipe(gate), 0) << strerror(errno);
    std::vector<pid_t> children;
    constexpr size_t kWorkers = 4;
    for (size_t index = 0; index < kWorkers; ++index) {
        const pid_t child = fork();
        if (child < 0) {
            ADD_FAILURE() << "fork: " << strerror(errno);
            break;
        }
        if (child == 0) {
            close(gate[1]);
            char start;
            ssize_t count;
            do {
                count = read(gate[0], &start, 1);
            } while (count < 0 && errno == EINTR);
            close(gate[0]);
            if (count != 1) {
                _exit(10);
            }
            if (!BindCpu(cpus[index % cpus.size()])) {
                _exit(11);
            }
            const auto first = ReadReport();
            const auto second = ReadReport();
            _exit(ReportOk(first) && first == second ? 0 : 12);
        }
        children.push_back(child);
    }
    close(gate[0]);
    for (size_t index = 0; index < children.size(); ++index) {
        ssize_t count;
        do {
            count = write(gate[1], "s", 1);
        } while (count < 0 && errno == EINTR);
        EXPECT_EQ(count, 1) << strerror(errno);
    }
    close(gate[1]);
    for (const pid_t child : children) {
        int status = 0;
        pid_t waited;
        do {
            waited = waitpid(child, &status, 0);
        } while (waited < 0 && errno == EINTR);
        EXPECT_EQ(waited, child) << strerror(errno);
        EXPECT_TRUE(WIFEXITED(status)) << "pid=" << child << " status=" << status;
        if (WIFEXITED(status)) {
            EXPECT_EQ(WEXITSTATUS(status), 0) << "pid=" << child;
        }
    }
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
