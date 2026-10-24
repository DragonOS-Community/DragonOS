#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include <string>

namespace {
std::string ReadReport(int fd, size_t chunk) {
    std::string result;
    char buffer[256];
    for (;;) {
        const ssize_t count = read(fd, buffer, chunk);
        if (count < 0 && errno == EINTR) continue;
        if (count == 0) return result;
        if (count < 0) {
            ADD_FAILURE() << strerror(errno);
            return result;
        }
        result.append(buffer, static_cast<size_t>(count));
    }
}
}  // namespace

TEST(MobilityPlacement, RealChurnAndBootReservationPreserveOwnership) {
    constexpr const char* path = "/sys/kernel/debug/mm/placement_selftest";
    struct stat info;
    if (stat(path, &info) != 0) {
        if (errno == ENOENT) GTEST_SKIP() << "requires x86_64 KERNEL_FEATURES=mm_selftest";
        FAIL() << strerror(errno);
    }
    const int fd = open(path, O_RDONLY | O_CLOEXEC);
    ASSERT_GE(fd, 0) << strerror(errno);
    const auto report = ReadReport(fd, 7);
    ASSERT_EQ(lseek(fd, 0, SEEK_SET), 0);
    EXPECT_EQ(ReadReport(fd, 256), report);
    EXPECT_EQ(close(fd), 0);
    for (const char* check : {"placement_churn=ok\n", "boot_reservation=ok\n",
                              "placement_boundaries=ok\n", "placement_metadata=ok\n",
                              "consumer_births=ok\n", "status=ok\n", "summary_fail=0\n"}) {
        EXPECT_NE(report.find(check), std::string::npos) << report;
    }
}

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
