#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <unistd.h>

#include <sstream>
#include <string>
#include <vector>

namespace {

struct ExpectedRootMountOptions {
    std::string mode = "ro";
    bool sync = false;
    bool dirsync = false;
    bool lazytime = false;
    bool mand = false;
    bool nosuid = false;
    bool nodev = false;
    bool noexec = false;
    bool noatime = false;
    bool nodiratime = false;
    bool relatime = true;
    bool nosymfollow = false;
};

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

std::vector<std::string> split_commas(const std::string& input) {
    std::vector<std::string> out;
    size_t start = 0;
    while (start <= input.size()) {
        const size_t end = input.find(',', start);
        const size_t len = (end == std::string::npos) ? input.size() - start : end - start;
        out.push_back(input.substr(start, len));
        if (end == std::string::npos) {
            break;
        }
        start = end + 1;
    }
    return out;
}

bool has_option(const std::vector<std::string>& options, const char* expected) {
    for (const std::string& option : options) {
        if (option == expected) {
            return true;
        }
    }
    return false;
}

void apply_rootflag_option(const std::string& token, ExpectedRootMountOptions* expected) {
    if (token == "ro" || token == "rw") {
        expected->mode = token;
    } else if (token == "sync") {
        expected->sync = true;
    } else if (token == "async") {
        expected->sync = false;
    } else if (token == "dirsync") {
        expected->dirsync = true;
    } else if (token == "lazytime") {
        expected->lazytime = true;
    } else if (token == "nolazytime") {
        expected->lazytime = false;
    } else if (token == "mand") {
        expected->mand = true;
    } else if (token == "nomand") {
        expected->mand = false;
    } else if (token == "nosuid") {
        expected->nosuid = true;
    } else if (token == "suid") {
        expected->nosuid = false;
    } else if (token == "nodev") {
        expected->nodev = true;
    } else if (token == "dev") {
        expected->nodev = false;
    } else if (token == "noexec") {
        expected->noexec = true;
    } else if (token == "exec") {
        expected->noexec = false;
    } else if (token == "noatime") {
        expected->noatime = true;
        expected->relatime = false;
    } else if (token == "atime" || token == "strictatime") {
        expected->noatime = false;
        expected->relatime = false;
    } else if (token == "relatime") {
        expected->noatime = false;
        expected->relatime = true;
    } else if (token == "nodiratime") {
        expected->nodiratime = true;
    } else if (token == "diratime") {
        expected->nodiratime = false;
    } else if (token == "nosymfollow") {
        expected->nosymfollow = true;
    } else if (token == "symfollow") {
        expected->nosymfollow = false;
    }
}

ExpectedRootMountOptions expected_root_options_from_cmdline(const std::string& cmdline) {
    ExpectedRootMountOptions expected;

    for (const std::string& token : split_whitespace(cmdline)) {
        if (token == "--") {
            break;
        }

        if (token == "ro" || token == "rw") {
            expected.mode = token;
        }

        constexpr const char* kRootflags = "rootflags=";
        if (token.rfind(kRootflags, 0) != 0) {
            continue;
        }

        std::string flags = token.substr(strlen(kRootflags));
        for (const std::string& option : split_commas(flags)) {
            apply_rootflag_option(option, &expected);
        }
    }

    return expected;
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

void expect_option(const std::vector<std::string>& options, const char* option) {
    EXPECT_TRUE(has_option(options, option)) << "missing mount option: " << option;
}

void expect_no_option(const std::vector<std::string>& options, const char* option) {
    EXPECT_FALSE(has_option(options, option)) << "unexpected mount option: " << option;
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

    const ExpectedRootMountOptions expected = expected_root_options_from_cmdline(cmdline);
    ASSERT_GE(options.size(), expected.mode.size()) << "root mount options: " << options;
    EXPECT_EQ(expected.mode, options.substr(0, expected.mode.size()))
        << "cmdline:\n" << cmdline << "\n/proc/self/mounts:\n" << mounts;

    const std::vector<std::string> option_tokens = split_commas(options);
    if (expected.sync) expect_option(option_tokens, "sync");
    if (expected.dirsync) expect_option(option_tokens, "dirsync");
    if (expected.lazytime) expect_option(option_tokens, "lazytime");
    if (expected.mand) expect_option(option_tokens, "mand");
    if (expected.nosuid) expect_option(option_tokens, "nosuid");
    if (expected.nodev) expect_option(option_tokens, "nodev");
    if (expected.noexec) expect_option(option_tokens, "noexec");
    if (expected.noatime) expect_option(option_tokens, "noatime");
    if (expected.nodiratime) expect_option(option_tokens, "nodiratime");
    if (expected.relatime) expect_option(option_tokens, "relatime");
    if (expected.nosymfollow) expect_option(option_tokens, "nosymfollow");
    if (expected.noatime) expect_no_option(option_tokens, "relatime");
    if (!expected.noatime && !expected.relatime) {
        expect_no_option(option_tokens, "noatime");
        expect_no_option(option_tokens, "relatime");
    }
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
