#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <unistd.h>

#include <sstream>
#include <string>
#include <vector>

namespace {

bool read_file(const char* path, std::string* out) {
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

std::vector<std::string> kernel_param_tokens(const std::string& cmdline) {
    std::vector<std::string> out;
    for (const std::string& token : split_whitespace(cmdline)) {
        if (token == "--") {
            break;
        }
        out.push_back(token);
    }
    return out;
}

std::vector<std::string> split_nul(const std::string& input) {
    std::vector<std::string> out;
    size_t start = 0;
    while (start < input.size()) {
        const size_t end = input.find('\0', start);
        const size_t len = (end == std::string::npos) ? input.size() - start : end - start;
        if (len != 0) {
            out.push_back(input.substr(start, len));
        }
        if (end == std::string::npos) {
            break;
        }
        start = end + 1;
    }
    return out;
}

bool contains_token(const std::vector<std::string>& tokens, const std::string& token) {
    for (const std::string& candidate : tokens) {
        if (candidate == token) {
            return true;
        }
    }
    return false;
}

bool has_prefix_token(const std::vector<std::string>& tokens, const std::string& prefix) {
    for (const std::string& candidate : tokens) {
        if (candidate.rfind(prefix, 0) == 0) {
            return true;
        }
    }
    return false;
}

}  // namespace

TEST(KernelCmdlineCompat, LinuxKnownBareParametersDoNotReachInitArgv) {
    std::string kernel_cmdline;
    ASSERT_TRUE(read_file("/proc/cmdline", &kernel_cmdline))
        << "read /proc/cmdline failed: errno=" << errno << " (" << strerror(errno) << ")";

    std::string init_cmdline;
    ASSERT_TRUE(read_file("/proc/1/cmdline", &init_cmdline))
        << "read /proc/1/cmdline failed: errno=" << errno << " (" << strerror(errno) << ")";

    const std::vector<std::string> kernel_tokens = kernel_param_tokens(kernel_cmdline);
    const std::vector<std::string> init_argv = split_nul(init_cmdline);
    const std::vector<std::string> linux_bare_params = {
        "no_timer_check",
        "no-timer-check",
        "noreplace-smp",
        "noreplace_smp",
        "quiet",
    };

    bool saw_target_param = false;
    for (const std::string& param : linux_bare_params) {
        if (!contains_token(kernel_tokens, param)) {
            continue;
        }
        saw_target_param = true;
        EXPECT_FALSE(contains_token(init_argv, param))
            << param << " leaked into /proc/1/cmdline";
    }

    if (!saw_target_param) {
        GTEST_SKIP() << "kernel cmdline does not contain CubeSandbox Linux bare parameters";
    }
}

TEST(KernelCmdlineCompat, AgentDottedParametersStayOutOfInitArgv) {
    std::string kernel_cmdline;
    ASSERT_TRUE(read_file("/proc/cmdline", &kernel_cmdline))
        << "read /proc/cmdline failed: errno=" << errno << " (" << strerror(errno) << ")";

    std::string init_cmdline;
    ASSERT_TRUE(read_file("/proc/1/cmdline", &init_cmdline))
        << "read /proc/1/cmdline failed: errno=" << errno << " (" << strerror(errno) << ")";

    const std::vector<std::string> kernel_tokens = kernel_param_tokens(kernel_cmdline);
    const std::vector<std::string> init_argv = split_nul(init_cmdline);
    const std::vector<std::string> agent_prefixes = {
        "agent.debug_console",
        "agent.debug_console_vport=",
        "agent.log=",
        "agent.log_vport=",
    };

    bool saw_agent_param = false;
    for (const std::string& prefix : agent_prefixes) {
        if (!has_prefix_token(kernel_tokens, prefix)) {
            continue;
        }
        saw_agent_param = true;
        EXPECT_FALSE(has_prefix_token(init_argv, prefix))
            << prefix << " leaked into /proc/1/cmdline";
    }

    if (!saw_agent_param) {
        GTEST_SKIP() << "kernel cmdline does not contain CubeSandbox agent dotted parameters";
    }
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
