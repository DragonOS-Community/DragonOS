#include <gtest/gtest.h>

#include <array>
#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <string>
#include <unistd.h>

namespace {

std::string ReadFdLink(int fd, size_t buf_size = 128) {
    std::string proc_path = "/proc/self/fd/" + std::to_string(fd);
    std::string target(buf_size, '\0');
    ssize_t len = readlink(proc_path.c_str(), target.data(), target.size());
    if (len < 0) {
        return "";
    }
    target.resize(static_cast<size_t>(len));
    return target;
}

void ExpectProcFdTarget(const char* path) {
    int fd = open(path, O_RDONLY);
    ASSERT_GE(fd, 0) << "open(" << path << ") failed: errno=" << errno << " ("
                     << std::strerror(errno) << ")";

    EXPECT_EQ(path, ReadFdLink(fd));
    EXPECT_EQ(std::string(path).substr(0, 1), ReadFdLink(fd, 1));

    std::string proc_path = "/proc/self/fd/" + std::to_string(fd);
    std::array<char, 1> dummy {};
    errno = 0;
    ssize_t len = readlink(proc_path.c_str(), dummy.data(), 0);
    EXPECT_EQ(-1, len);
    EXPECT_EQ(EINVAL, errno) << "zero-sized readlink failed with errno=" << errno
                             << " (" << std::strerror(errno) << ")";

    EXPECT_EQ(0, close(fd));
}

void ExpectCharDeviceNumber(const char* path, unsigned int major_num, unsigned int minor_num) {
    struct stat st = {};
    ASSERT_EQ(0, stat(path, &st)) << "stat(" << path << ") failed: errno=" << errno << " ("
                                  << std::strerror(errno) << ")";

    EXPECT_TRUE(S_ISCHR(st.st_mode)) << path << " is not a character device";
    EXPECT_EQ(major_num, major(st.st_rdev)) << path << " major mismatch";
    EXPECT_EQ(minor_num, minor(st.st_rdev)) << path << " minor mismatch";
}

void ExpectReadZeros(const char* path) {
    int fd = open(path, O_RDONLY);
    ASSERT_GE(fd, 0) << "open(" << path << ") failed: errno=" << errno << " ("
                     << std::strerror(errno) << ")";

    std::array<unsigned char, 16> buf {};
    buf.fill(0xff);
    ASSERT_EQ(static_cast<ssize_t>(buf.size()), read(fd, buf.data(), buf.size()))
        << "read(" << path << ") failed: errno=" << errno << " (" << std::strerror(errno)
        << ")";
    for (unsigned char byte : buf) {
        EXPECT_EQ(0, byte);
    }

    EXPECT_EQ(0, close(fd));
}

}  // namespace

TEST(ProcFdDevfsReadlink, BuiltinCharacterDevicesResolveToDevPaths) {
    ExpectProcFdTarget("/dev/null");
    ExpectProcFdTarget("/dev/zero");
    ExpectProcFdTarget("/dev/full");
    ExpectProcFdTarget("/dev/random");
    ExpectProcFdTarget("/dev/urandom");
}

TEST(ProcFdDevfsReadlink, BuiltinCharacterDevicesExposeLinuxDeviceNumbers) {
    ExpectCharDeviceNumber("/dev/null", 1, 3);
    ExpectCharDeviceNumber("/dev/zero", 1, 5);
    ExpectCharDeviceNumber("/dev/full", 1, 7);
    ExpectCharDeviceNumber("/dev/random", 1, 8);
    ExpectCharDeviceNumber("/dev/urandom", 1, 9);
}

TEST(ProcFdDevfsReadlink, FullDeviceMatchesLinuxReadWriteSemantics) {
    ExpectReadZeros("/dev/full");

    int fd = open("/dev/full", O_WRONLY);
    ASSERT_GE(fd, 0) << "open(/dev/full) failed: errno=" << errno << " ("
                     << std::strerror(errno) << ")";

    const char data[] = "x";
    errno = 0;
    ssize_t written = write(fd, data, sizeof(data));
    EXPECT_EQ(-1, written);
    EXPECT_EQ(ENOSPC, errno) << "write(/dev/full) failed with errno=" << errno << " ("
                             << std::strerror(errno) << ")";

    EXPECT_EQ(0, close(fd));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
