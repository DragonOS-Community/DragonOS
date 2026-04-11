#include <gtest/gtest.h>

#include <sys/resource.h>

#include <algorithm>
#include <cstdint>
#include <cstring>
#include <fstream>
#include <string>

namespace {

constexpr const char* kProcSelfLimits = "/proc/self/limits";

std::string Trim(std::string s) {
    auto not_space = [](unsigned char c) {
        return c != ' ' && c != '\t' && c != '\n' && c != '\r';
    };

    s.erase(s.begin(), std::find_if(s.begin(), s.end(), not_space));
    s.erase(std::find_if(s.rbegin(), s.rend(), not_space).base(), s.end());
    return s;
}

std::string FormatLimitValue(rlim_t value) {
    if (value == RLIM_INFINITY || value == static_cast<rlim_t>(-1)) {
        return "unlimited";
    }
    return std::to_string(static_cast<unsigned long long>(value));
}

bool ReadNoFileFromProc(std::string* soft, std::string* hard) {
    std::ifstream in(kProcSelfLimits);
    if (!in.is_open()) {
        return false;
    }

    std::string line;
    while (std::getline(in, line)) {
        if (line.rfind("Max open files", 0) != 0) {
            continue;
        }

        if (line.size() < 67) {
            return false;
        }

        *soft = Trim(line.substr(26, 20));
        *hard = Trim(line.substr(47, 20));
        return true;
    }

    return false;
}

}  // namespace

TEST(ProcSelfLimits, ReflectsSetRlimitNoFile) {
    struct rlimit old_limit {};
    ASSERT_EQ(0, getrlimit(RLIMIT_NOFILE, &old_limit))
        << "getrlimit failed: errno=" << errno << " (" << std::strerror(errno) << ")";

    struct rlimit new_limit = old_limit;
    if (old_limit.rlim_cur == 0 || old_limit.rlim_cur == RLIM_INFINITY) {
        rlim_t candidate = 1024;
        if (old_limit.rlim_max != RLIM_INFINITY && candidate > old_limit.rlim_max) {
            candidate = old_limit.rlim_max;
        }
        new_limit.rlim_cur = candidate;
    } else {
        new_limit.rlim_cur = old_limit.rlim_cur - 1;
    }

    ASSERT_EQ(0, setrlimit(RLIMIT_NOFILE, &new_limit))
        << "setrlimit failed: errno=" << errno << " (" << std::strerror(errno) << ")";

    std::string proc_soft;
    std::string proc_hard;
    ASSERT_TRUE(ReadNoFileFromProc(&proc_soft, &proc_hard))
        << "failed to parse 'Max open files' from " << kProcSelfLimits;

    const std::string expect_soft = FormatLimitValue(new_limit.rlim_cur);
    const std::string expect_hard = FormatLimitValue(new_limit.rlim_max);

    EXPECT_EQ(expect_soft, proc_soft);
    EXPECT_EQ(expect_hard, proc_hard);

    EXPECT_EQ(0, setrlimit(RLIMIT_NOFILE, &old_limit))
        << "restore setrlimit failed: errno=" << errno << " (" << std::strerror(errno) << ")";
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
