#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <unistd.h>

#include <sstream>
#include <string>
#include <vector>

namespace {

bool read_text_file(const char* path, std::string* out) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return false;
    }

    out->clear();
    char buf[1024];
    ssize_t n = 0;
    while ((n = read(fd, buf, sizeof(buf))) > 0) {
        out->append(buf, static_cast<size_t>(n));
    }

    const int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return n >= 0;
}

std::vector<std::string> split_whitespace(const std::string& input) {
    std::istringstream stream(input);
    std::vector<std::string> out;
    std::string token;
    while (stream >> token) {
        out.push_back(token);
    }
    return out;
}

void apply_rw_token(const std::string& token, std::string* mode) {
    if (token == "ro" || token == "rw") {
        *mode = token;
    }
}

std::string expected_root_mode_from_cmdline(const std::string& cmdline) {
    std::string mode = "ro";

    for (const std::string& token : split_whitespace(cmdline)) {
        if (token == "--") {
            break;
        }

        apply_rw_token(token, &mode);

        constexpr const char* kRootflags = "rootflags=";
        if (token.rfind(kRootflags, 0) != 0) {
            continue;
        }

        std::string flags = token.substr(strlen(kRootflags));
        size_t start = 0;
        while (start <= flags.size()) {
            const size_t end = flags.find(',', start);
            const size_t len = (end == std::string::npos) ? flags.size() - start : end - start;
            apply_rw_token(flags.substr(start, len), &mode);
            if (end == std::string::npos) {
                break;
            }
            start = end + 1;
        }
    }

    return mode;
}

bool find_root_mount_options(const std::string& mounts, std::string* options) {
    std::istringstream lines(mounts);
    std::string line;
    while (std::getline(lines, line)) {
        std::istringstream fields(line);
        std::string source;
        std::string mountpoint;
        std::string fstype;
        if (!(fields >> source >> mountpoint >> fstype >> *options)) {
            continue;
        }
        if (mountpoint == "/") {
            return true;
        }
    }
    return false;
}

}  // namespace

TEST(RootMountCmdline, RootMountModeFollowsKernelCommandLine) {
    std::string cmdline;
    ASSERT_TRUE(read_text_file("/proc/cmdline", &cmdline))
        << "read /proc/cmdline failed: errno=" << errno << " (" << strerror(errno) << ")";

    std::string mounts;
    ASSERT_TRUE(read_text_file("/proc/self/mounts", &mounts))
        << "read /proc/self/mounts failed: errno=" << errno << " (" << strerror(errno) << ")";

    std::string options;
    ASSERT_TRUE(find_root_mount_options(mounts, &options)) << "/proc/self/mounts:\n" << mounts;

    const std::string expected = expected_root_mode_from_cmdline(cmdline);
    ASSERT_GE(options.size(), expected.size()) << "root mount options: " << options;
    EXPECT_EQ(expected, options.substr(0, expected.size()))
        << "cmdline:\n" << cmdline << "\n/proc/self/mounts:\n" << mounts;
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
