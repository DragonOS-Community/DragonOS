#include <gtest/gtest.h>

#include <fcntl.h>
#include <time.h>
#include <unistd.h>

#include <regex>
#include <string>

namespace {

double Seconds(const timespec& ts) {
    return static_cast<double>(ts.tv_sec) + ts.tv_nsec / 1e9;
}

TEST(ProcUptime, FormatAndBoottimeClock) {
    timespec before {}, after {};
    ASSERT_EQ(0, clock_gettime(CLOCK_BOOTTIME, &before));
    int fd = open("/proc/uptime", O_RDONLY);
    ASSERT_GE(fd, 0);
    char buffer[128];
    ssize_t size = read(fd, buffer, sizeof(buffer));
    close(fd);
    ASSERT_GT(size, 0);
    ASSERT_EQ(0, clock_gettime(CLOCK_BOOTTIME, &after));
    std::string line(buffer, size);
    ASSERT_TRUE(std::regex_match(line, std::regex("[0-9]+\\.[0-9]{2} [0-9]+\\.[0-9]{2}\\n"))) << line;
    double uptime = std::stod(line);
    // /proc/uptime truncates to hundredths of a second.
    EXPECT_GE(uptime + 0.010001, Seconds(before));
    EXPECT_LE(uptime, Seconds(after));
}

TEST(ProcUptime, PartialReadsAndRewind) {
    int fd = open("/proc/uptime", O_RDONLY);
    ASSERT_GE(fd, 0);
    std::string line;
    char byte;
    ssize_t count;
    while ((count = read(fd, &byte, 1)) == 1 && line.size() < 128) {
        line.push_back(byte);
    }
    EXPECT_EQ(0, count);
    EXPECT_TRUE(std::regex_match(line, std::regex("[0-9]+\\.[0-9]{2} [0-9]+\\.[0-9]{2}\\n")));
    EXPECT_EQ(0, lseek(fd, 0, SEEK_SET));
    char buffer[128];
    ssize_t size = read(fd, buffer, sizeof(buffer));
    close(fd);
    ASSERT_GT(size, 0);
    EXPECT_TRUE(std::regex_match(std::string(buffer, size),
                                std::regex("[0-9]+\\.[0-9]{2} [0-9]+\\.[0-9]{2}\\n")));
}

} // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
