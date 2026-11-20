#include <gtest/gtest.h>

#include <cctype>
#include <cstdio>
#include <cstdlib>
#include <cstdint>
#include <fstream>
#include <map>
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

struct CpuTopology {
    int package;
    int core;
};

void ExpectMask(const std::string& path, const std::vector<int>& cpus) {
    std::string mask;
    ASSERT_TRUE(ReadFile(path, &mask)) << path;
    ASSERT_FALSE(mask.empty()) << path;
    EXPECT_EQ('\n', mask.back()) << path;
    ASSERT_NE(std::string::npos, mask.find_first_of("0123456789abcdef")) << path;
    EXPECT_EQ(mask.size() - 1, mask.find_first_not_of("0123456789abcdef")) << path;

    uint64_t expected = 0;
    for (int cpu : cpus) {
        ASSERT_LT(cpu, 64) << path;
        expected |= UINT64_C(1) << cpu;
    }
    EXPECT_EQ(expected, std::stoull(mask, nullptr, 16)) << path;
}

}  // namespace

TEST(CpuTopologySysfs, PresentAndTopologyAbi) {
    const std::string root = "/sys/devices/system/cpu/";
    std::string present;
    ASSERT_TRUE(ReadFile(root + "present", &present));
    ASSERT_FALSE(present.empty());
    EXPECT_EQ('\n', present.back());

    const auto cpus = ParseCpuList(present);
    ASSERT_FALSE(cpus.empty());
    std::map<int, CpuTopology> topology;
    std::map<int, std::vector<int>> packages;
    std::map<std::pair<int, int>, std::vector<int>> cores;

    for (int cpu : cpus) {
        const std::string base = root + "cpu" + std::to_string(cpu) + "/topology/";
        std::string package_id;
        std::string core_id;
        ASSERT_TRUE(ReadFile(base + "physical_package_id", &package_id));
        ASSERT_TRUE(ReadFile(base + "core_id", &core_id));
        const int package = std::stoi(package_id);
        const int core = std::stoi(core_id);
        EXPECT_TRUE(topology.emplace(cpu, CpuTopology{package, core}).second);
        packages[package].push_back(cpu);
        cores[{package, core}].push_back(cpu);
    }

    std::map<int, size_t> cores_per_package;
    for (const auto& core : cores) {
        ++cores_per_package[core.first.first];
    }

    // The focused QEMU run supplies the requested VM topology independently
    // of the kernel output and the CPUID decoder under test.
    if (const char* expected = std::getenv("DUNITEST_EXPECT_CPU_TOPOLOGY")) {
        size_t sockets = 0, cores_per_socket = 0, threads_per_core = 0;
        char extra = '\0';
        ASSERT_EQ(3, std::sscanf(expected, "%zux%zux%zu%c",
                                 &sockets, &cores_per_socket, &threads_per_core, &extra));
        ASSERT_GT(sockets, 0u);
        ASSERT_GT(cores_per_socket, 0u);
        ASSERT_GT(threads_per_core, 0u);
        EXPECT_EQ(sockets * cores_per_socket * threads_per_core, cpus.size());
        EXPECT_EQ(sockets, packages.size());
        for (const auto& package : packages) {
            EXPECT_EQ(cores_per_socket, cores_per_package.at(package.first));
        }
        for (const auto& core : cores) {
            EXPECT_EQ(threads_per_core, core.second.size());
        }
    }

    for (const auto& [cpu, ids] : topology) {
        const std::string base = root + "cpu" + std::to_string(cpu) + "/topology/";
        const auto& package = packages.at(ids.package);
        const auto& core = cores.at({ids.package, ids.core});
        std::string list;
        ASSERT_TRUE(ReadFile(base + "thread_siblings_list", &list));
        EXPECT_EQ(core, ParseCpuList(list));
        ASSERT_TRUE(ReadFile(base + "core_siblings_list", &list));
        EXPECT_EQ(package, ParseCpuList(list));
        ExpectMask(base + "thread_siblings", core);
        ExpectMask(base + "core_siblings", package);
    }

    std::string cpuinfo;
    ASSERT_TRUE(ReadFile("/proc/cpuinfo", &cpuinfo));
    std::istringstream lines(cpuinfo);
    std::map<int, std::map<std::string, std::string>> records;
    std::string line;
    int cpu = -1;
    while (std::getline(lines, line)) {
        const auto colon = line.find(':');
        if (colon == std::string::npos) {
            continue;
        }

        const auto name = line.substr(0, line.find_last_not_of(" \t", colon - 1) + 1);
        const auto value = line.substr(colon + 1);
        if (name == "processor") {
            cpu = std::stoi(value);
        }
        if (cpu >= 0) {
            records[cpu][name] = value;
        }
    }

    ASSERT_EQ(topology.size(), records.size());
    for (const auto& [id, ids] : topology) {
        const auto record = records.find(id);
        ASSERT_NE(records.end(), record) << id;
        const auto& fields = record->second;
        for (const char* name : {"physical id", "core id", "siblings", "cpu cores", "cpu MHz"}) {
            ASSERT_EQ(1u, fields.count(name)) << id << ": " << name;
        }
        EXPECT_EQ(ids.package, std::stoi(fields.at("physical id")));
        EXPECT_EQ(ids.core, std::stoi(fields.at("core id")));
        EXPECT_EQ(packages.at(ids.package).size(), std::stoul(fields.at("siblings")));
        EXPECT_EQ(cores_per_package.at(ids.package), std::stoul(fields.at("cpu cores")));
        EXPECT_GT(std::stod(fields.at("cpu MHz")), 0.0);
    }

    const std::string clocksource = "/sys/devices/system/clocksource/clocksource0/";
    std::string available;
    std::string current;
    ASSERT_TRUE(ReadFile(clocksource + "available_clocksource", &available));
    ASSERT_TRUE(ReadFile(clocksource + "current_clocksource", &current));
    std::istringstream sources(available);
    std::istringstream selected(current);
    std::string name;
    ASSERT_TRUE(selected >> name);
    std::string source;
    while (sources >> source) {
        if (source == name) {
            return;
        }
    }
    ADD_FAILURE() << name << " is absent from available_clocksource: " << available;
}

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
