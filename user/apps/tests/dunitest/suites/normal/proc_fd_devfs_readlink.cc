#include <gtest/gtest.h>

#include <array>
#include <cerrno>
#include <cstring>
#include <fcntl.h>
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
    EXPECT_EQ(0, len) << "zero-sized readlink failed: errno=" << errno << " ("
                      << std::strerror(errno) << ")";

    EXPECT_EQ(0, close(fd));
}

}  // namespace

TEST(ProcFdDevfsReadlink, BuiltinCharacterDevicesResolveToDevPaths) {
    ExpectProcFdTarget("/dev/null");
    ExpectProcFdTarget("/dev/zero");
    ExpectProcFdTarget("/dev/random");
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
