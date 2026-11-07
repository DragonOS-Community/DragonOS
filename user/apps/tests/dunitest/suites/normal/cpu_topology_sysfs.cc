#include <gtest/gtest.h>

#include <fstream>
#include <sstream>
#include <string>

namespace {

bool ReadFile(const std::string& path, std::string* out) {
    std::ifstream file(path);
    if (!file) {
        return false;
    }
    std::ostringstream contents;
    contents << file.rdbuf();
    *out = contents.str();
    return true;
}

bool IsHexMask(const std::string& value) {
    if (value.empty()) {
        return false;
    }
    for (char ch : value) {
        if (ch == '\n') {
            continue;
        }
        if (!((ch >= '0' && ch <= '9') || (ch >= 'a' && ch <= 'f'))) {
            return false;
        }
    }
    return true;
}

}  // namespace

// Pin the Linux ABI used by OCM-Agent and lscpu: present CPU mask, per-CPU
// topology, /proc/cpuinfo topology fields, and the clocksource directory.
TEST(CpuTopologySysfs, PresentAndTopologyAbi) {
    std::string present;
    ASSERT_TRUE(ReadFile("/sys/devices/system/cpu/present", &present));
    EXPECT_FALSE(present.empty());
    EXPECT_EQ('\n', present.back());

    for (int cpu = 0; cpu < 2; ++cpu) {
        const std::string base =
            "/sys/devices/system/cpu/cpu" + std::to_string(cpu) + "/topology/";

        std::string core_id;
        ASSERT_TRUE(ReadFile(base + "core_id", &core_id));
        EXPECT_FALSE(core_id.empty());

        std::string package_id;
        ASSERT_TRUE(ReadFile(base + "physical_package_id", &package_id));
        EXPECT_FALSE(package_id.empty());

        for (const char* name : {"thread_siblings", "core_siblings"}) {
            std::string mask;
            ASSERT_TRUE(ReadFile(base + name, &mask)) << name;
            EXPECT_TRUE(IsHexMask(mask)) << name << "=" << mask;
        }

        for (const char* name : {"thread_siblings_list", "core_siblings_list"}) {
            std::string list;
            ASSERT_TRUE(ReadFile(base + name, &list)) << name;
            EXPECT_FALSE(list.empty()) << name;
        }
    }

    std::string cpuinfo;
    ASSERT_TRUE(ReadFile("/proc/cpuinfo", &cpuinfo));
    EXPECT_NE(std::string::npos, cpuinfo.find("physical id"));
    EXPECT_NE(std::string::npos, cpuinfo.find("core id"));
    EXPECT_NE(std::string::npos, cpuinfo.find("cpu cores"));

    bool found_mhz = false;
    size_t pos = 0;
    while ((pos = cpuinfo.find("cpu MHz", pos)) != std::string::npos) {
        const size_t colon = cpuinfo.find(':', pos);
        ASSERT_NE(std::string::npos, colon);
        const size_t newline = cpuinfo.find('\n', colon);
        ASSERT_NE(std::string::npos, newline);
        const std::string value = cpuinfo.substr(colon + 1, newline - colon - 1);
        EXPECT_GT(std::stod(value), 0.0);
        found_mhz = true;
        pos = newline;
    }
    EXPECT_TRUE(found_mhz);

    std::string available;
    ASSERT_TRUE(ReadFile(
        "/sys/devices/system/clocksource/clocksource0/available_clocksource", &available));
    EXPECT_FALSE(available.empty());

    std::string current;
    ASSERT_TRUE(ReadFile(
        "/sys/devices/system/clocksource/clocksource0/current_clocksource", &current));
    EXPECT_FALSE(current.empty());
}

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
