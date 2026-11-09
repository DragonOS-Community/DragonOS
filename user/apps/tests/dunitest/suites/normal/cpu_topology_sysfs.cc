#include <gtest/gtest.h>

#include <cpuid.h>

#include <algorithm>
#include <cctype>
#include <fstream>
#include <set>
#include <sstream>
#include <string>
#include <vector>

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

std::vector<int> ParseCpuList(const std::string& text) {
    std::vector<int> cpus;
    size_t pos = 0;
    while (pos < text.size()) {
        while (pos < text.size() && !std::isdigit(static_cast<unsigned char>(text[pos]))) {
            ++pos;
        }
        if (pos == text.size()) {
            break;
        }

        int start = 0;
        while (pos < text.size() && std::isdigit(static_cast<unsigned char>(text[pos]))) {
            start = start * 10 + (text[pos] - '0');
            ++pos;
        }

        int end = start;
        if (pos < text.size() && text[pos] == '-') {
            ++pos;
            end = 0;
            while (pos < text.size() && std::isdigit(static_cast<unsigned char>(text[pos]))) {
                end = end * 10 + (text[pos] - '0');
                ++pos;
            }
        }
        for (int cpu = start; cpu <= end; ++cpu) {
            cpus.push_back(cpu);
        }
    }
    return cpus;
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

struct SmtInfo {
    int shift = -1;
    size_t threads = 1;
};

// Returns shift=-1 when the leaf has no usable topology. A valid topology
// without an SMT level means one thread per core.
SmtInfo SmtInfoFromLeaf(unsigned int leaf) {
    for (unsigned int subleaf = 0; subleaf < 16; ++subleaf) {
        unsigned int eax = 0, ebx = 0, ecx = 0, edx = 0;
        __cpuid_count(leaf, subleaf, eax, ebx, ecx, edx);
        const unsigned int level_type = (ecx >> 8) & 0xff;

        if (level_type == 0) {
            if (subleaf == 0) {
                return {};
            }
            break;
        }
        if (level_type == 1) {
            const size_t processors = ebx & 0xffff;
            return {static_cast<int>(eax & 0x1f), processors ? processors : 1};
        }
        if (subleaf == 0) {
            return {0, 1};
        }
    }
    return {};
}

SmtInfo ExpectedSmtInfo() {
    SmtInfo info = SmtInfoFromLeaf(0x1f);
    if (info.shift < 0) {
        info = SmtInfoFromLeaf(0x0b);
    }
    if (info.shift < 0) {
        info = {0, 1};
    }
    return info;
}

}  // namespace

// Pin the Linux ABI used by OCM-Agent and lscpu: present CPU mask, per-CPU
// topology, /proc/cpuinfo topology fields, and the clocksource directory.
TEST(CpuTopologySysfs, PresentAndTopologyAbi) {
    std::string present;
    ASSERT_TRUE(ReadFile("/sys/devices/system/cpu/present", &present));
    EXPECT_FALSE(present.empty());
    EXPECT_EQ('\n', present.back());

    const std::vector<int> cpus = ParseCpuList(present);
    ASSERT_FALSE(cpus.empty());

    const SmtInfo smt = ExpectedSmtInfo();
    const size_t threads_per_core = smt.threads;
    std::set<int> core_ids;

    for (int cpu : cpus) {
        const std::string base =
            "/sys/devices/system/cpu/cpu" + std::to_string(cpu) + "/topology/";

        std::string core_id_text;
        ASSERT_TRUE(ReadFile(base + "core_id", &core_id_text));
        const int core_id = std::stoi(core_id_text);
        core_ids.insert(core_id);

        std::string package_id_text;
        ASSERT_TRUE(ReadFile(base + "physical_package_id", &package_id_text));
        EXPECT_EQ(0, std::stoi(package_id_text));

        std::string thread_list_text;
        ASSERT_TRUE(ReadFile(base + "thread_siblings_list", &thread_list_text));
        const std::vector<int> thread_list = ParseCpuList(thread_list_text);
        EXPECT_NE(thread_list.end(), std::find(thread_list.begin(), thread_list.end(), cpu));
        EXPECT_EQ(threads_per_core, thread_list.size());

        std::string core_list_text;
        ASSERT_TRUE(ReadFile(base + "core_siblings_list", &core_list_text));
        EXPECT_EQ(cpus, ParseCpuList(core_list_text));

        for (const char* name : {"thread_siblings", "core_siblings"}) {
            std::string mask;
            ASSERT_TRUE(ReadFile(base + name, &mask)) << name;
            EXPECT_TRUE(IsHexMask(mask)) << name << "=" << mask;
        }
    }

    EXPECT_EQ(cpus.size() / threads_per_core, core_ids.size());

    std::string cpuinfo;
    ASSERT_TRUE(ReadFile("/proc/cpuinfo", &cpuinfo));
    EXPECT_NE(std::string::npos, cpuinfo.find("physical id"));
    EXPECT_NE(std::string::npos, cpuinfo.find("core id"));

    bool found_cores = false;
    size_t pos = 0;
    while ((pos = cpuinfo.find("cpu cores", pos)) != std::string::npos) {
        const size_t colon = cpuinfo.find(':', pos);
        ASSERT_NE(std::string::npos, colon);
        const size_t newline = cpuinfo.find('\n', colon);
        ASSERT_NE(std::string::npos, newline);
        EXPECT_EQ(core_ids.size(),
                  static_cast<size_t>(std::stoi(cpuinfo.substr(colon + 1, newline - colon - 1))));
        found_cores = true;
        pos = newline;
    }
    EXPECT_TRUE(found_cores);

    bool found_mhz = false;
    pos = 0;
    while ((pos = cpuinfo.find("cpu MHz", pos)) != std::string::npos) {
        const size_t colon = cpuinfo.find(':', pos);
        ASSERT_NE(std::string::npos, colon);
        const size_t newline = cpuinfo.find('\n', colon);
        ASSERT_NE(std::string::npos, newline);
        EXPECT_GT(std::stod(cpuinfo.substr(colon + 1, newline - colon - 1)), 0.0);
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
