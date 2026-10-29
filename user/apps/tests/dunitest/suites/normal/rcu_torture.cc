#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#include <string>

namespace {

constexpr const char* kTorturePath = "/sys/kernel/debug/rcu/torture";
constexpr size_t kRounds = 256;

std::string ReadAll() {
    int fd = open(kTorturePath, O_RDONLY);
    EXPECT_GE(fd, 0) << strerror(errno);
    if (fd < 0) {
        return {};
    }
    std::string report;
    char buf[512];
    while (true) {
        ssize_t n = read(fd, buf, sizeof(buf));
        if (n == 0) {
            break;
        }
        EXPECT_GT(n, 0) << strerror(errno);
        if (n < 0) {
            break;
        }
        report.append(buf, static_cast<size_t>(n));
    }
    EXPECT_EQ(0, close(fd));
    return report;
}

uint64_t Field(const std::string& report, const char* name) {
    const std::string prefix = std::string(name) + "=";
    size_t start = report.find(prefix);
    EXPECT_NE(std::string::npos, start) << report;
    if (start == std::string::npos) {
        return 0;
    }
    start += prefix.size();
    size_t end = report.find('\n', start);
    const std::string value = report.substr(start, end - start);
    char* parse_end = nullptr;
    errno = 0;
    unsigned long long parsed = strtoull(value.c_str(), &parse_end, 0);
    EXPECT_EQ(0, errno) << value;
    EXPECT_NE(parse_end, value.c_str()) << value;
    EXPECT_EQ('\0', *parse_end) << value;
    return static_cast<uint64_t>(parsed);
}

void RunSeed(uint64_t seed) {
    char command[96];
    int length = snprintf(command, sizeof(command), "seed=0x%016llx rounds=%zu",
                          static_cast<unsigned long long>(seed), kRounds);
    ASSERT_GT(length, 0);
    ASSERT_LT(static_cast<size_t>(length), sizeof(command));

    int fd = open(kTorturePath, O_WRONLY);
    ASSERT_GE(fd, 0) << strerror(errno);
    ssize_t written = write(fd, command, static_cast<size_t>(length));
    int write_errno = errno;
    EXPECT_EQ(0, close(fd));

    const std::string report = ReadAll();
    ASSERT_FALSE(report.empty());
    ASSERT_EQ(static_cast<ssize_t>(length), written)
        << "write errno=" << write_errno << " (" << strerror(write_errno) << ")\n" << report;
    EXPECT_NE(std::string::npos, report.find("status=ok\n")) << report;
    EXPECT_EQ(seed, Field(report, "seed")) << report;
    EXPECT_EQ(kRounds, Field(report, "rounds")) << report;
    EXPECT_EQ(kRounds, Field(report, "publishes")) << report;
    const long online_cpus = sysconf(_SC_NPROCESSORS_ONLN);
    ASSERT_GT(online_cpus, 0) << "sysconf(_SC_NPROCESSORS_ONLN) failed: errno=" << errno << " ("
                              << strerror(errno) << ")";
    const uint64_t reported_cpus = Field(report, "cpus");
    const uint64_t readers = Field(report, "readers");
    EXPECT_EQ(static_cast<uint64_t>(online_cpus), reported_cpus) << report;
    EXPECT_GE(readers, 1u) << report;
    EXPECT_LE(readers, reported_cpus) << report;
    if (online_cpus > 1) {
        EXPECT_GT(readers, 1u) << report;
    }
    EXPECT_GT(Field(report, "reads"), 0u) << report;
    EXPECT_GT(Field(report, "synchronize_calls"), 0u) << report;
    EXPECT_GT(Field(report, "barrier_calls"), 0u) << report;

    const uint64_t admitted = Field(report, "callbacks_admitted");
    const uint64_t invoked = Field(report, "callbacks_invoked");
    const uint64_t sync_reclaims = Field(report, "sync_reclaims");
    EXPECT_EQ(admitted, invoked) << report;
    EXPECT_EQ(kRounds + 1, invoked + sync_reclaims) << report;
    EXPECT_EQ(0u, Field(report, "premature_reclaims")) << report;
    EXPECT_EQ(0u, Field(report, "duplicate_reclaims")) << report;
    EXPECT_EQ(0u, Field(report, "corrupt_reads")) << report;
}

TEST(RcuTorture, ReproducibleBoundedSeeds) {
    RunSeed(0x1);
    RunSeed(0xdeadbeefcafebabeULL);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
