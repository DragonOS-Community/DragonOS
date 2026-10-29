#include <gtest/gtest.h>
#include <errno.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {
enum class Scenario { Whole, Middle, Adjacent, Prefix, Suffix, DoFork, Hint, NoReplace, FixedFailure, Grandchild };

void* Map(void* hint, size_t length, int flags = 0) {
    return mmap(hint, length, PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | flags, -1, 0);
}

int Wait(pid_t pid) {
    if (pid < 0) return 90;
    int status = 0;
    while (waitpid(pid, &status, 0) < 0) {
        if (errno != EINTR) return 91;
    }
    return WIFEXITED(status) ? WEXITSTATUS(status) : 92;
}

// No logging, allocation or other new VMA between these probes. mmap's exact
// address is not an ABI: only detect a free range becoming discoverable after
// a hint mapping and unmapping that leaves the address-space layout unchanged.
int CheckDiscoverability(char* excluded, size_t length) {
    void* first = Map(nullptr, length);
    if (first == MAP_FAILED) return 20;
    if (munmap(first, length)) return 21;
    void* hint = Map(excluded, length);
    if (hint != excluded) return 22;
    if (munmap(hint, length)) return 23;
    void* second = Map(nullptr, length);
    if (second == MAP_FAILED) return 24;
    const bool newly_discovered = first != excluded && second == excluded;
    if (munmap(second, length)) return 25;
    return newly_discovered ? 26 : 0;
}

int Probe(Scenario scenario, char* mapping, size_t pages, size_t first, size_t count, size_t page) {
    char* excluded = mapping + first * page;
    size_t length = count * page;
    unsigned char resident = 0;
    errno = 0;
    int result = mincore(excluded, page, &resident);
    if (scenario == Scenario::DoFork) {
        if (result || excluded[0] != static_cast<char>(41 + first)) return 10;
        excluded[0] = 99;
        errno = 0;
        if (Map(excluded, length, MAP_FIXED_NOREPLACE) != MAP_FAILED || errno != EEXIST) return 11;
        return 0;
    }
    if (result != -1 || errno != ENOMEM) return 12;
    // Adjacent surviving private mappings retain data and COW independence.
    for (size_t i = 0; i < pages; ++i) {
        if (i >= first && i < first + count) continue;
        if (mapping[i * page] != static_cast<char>(41 + i)) return 13;
        mapping[i * page] = 98;
    }
    if (scenario == Scenario::Grandchild) {
        pid_t child = fork();
        if (!child) _exit(CheckDiscoverability(excluded, length));
        return Wait(child);
    }
    if (scenario == Scenario::Hint || scenario == Scenario::NoReplace) {
        void* p = Map(excluded, length, scenario == Scenario::NoReplace ? MAP_FIXED_NOREPLACE : 0);
        if (p != excluded) return 14;
        static_cast<char*>(p)[0] = 97;
        return munmap(p, length) ? 15 : 0;
    }
    if (scenario == Scenario::FixedFailure) {
        // Overlap a surviving neighbor, leaving the excluded free page intact.
        errno = 0;
        if (Map(excluded, length + page, MAP_FIXED_NOREPLACE) != MAP_FAILED || errno != EEXIST) return 16;
        if (mapping[(first + count) * page] != 98) return 17;
    }
    return CheckDiscoverability(excluded, length);
}

int Exercise(Scenario scenario) {
    const size_t page = sysconf(_SC_PAGESIZE);
    const size_t pages = scenario == Scenario::Whole ? 1 : 4;
    size_t first = scenario == Scenario::Whole || scenario == Scenario::Prefix ? 0 : 1;
    size_t count = scenario == Scenario::Adjacent ? 2 : 1;
    if (scenario == Scenario::Suffix) first = pages - 1;
    char* mapping = static_cast<char*>(Map(nullptr, pages * page));
    if (mapping == MAP_FAILED) return 1;
    for (size_t i = 0; i < pages; ++i) mapping[i * page] = 41 + i;
    // Separate calls also exercise merging of adjacent excluded VMA intervals.
    for (size_t i = first; i < first + count; ++i) {
        if (madvise(mapping + i * page, page, MADV_DONTFORK)) return 2;
    }
    if (scenario == Scenario::DoFork && madvise(mapping + first * page, count * page, MADV_DOFORK)) return 3;
    pid_t child = fork();
    if (!child) _exit(Probe(scenario, mapping, pages, first, count, page));
    int result = Wait(child);
    for (size_t i = 0; i < pages; ++i) {
        if (mapping[i * page] != static_cast<char>(41 + i)) result = 4;
    }
    if (munmap(mapping, pages * page)) return 5;
    return result;
}

void RunScenario(Scenario scenario) {
    // Keep every layout isolated from gtest's allocations and prior test VMAs.
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (!child) _exit(Exercise(scenario));
    EXPECT_EQ(Wait(child), 0);
}
TEST(ForkMappingHoles, EntireDontForkMapping) { RunScenario(Scenario::Whole); }
TEST(ForkMappingHoles, MiddleDontForkAndNeighborCow) { RunScenario(Scenario::Middle); }
TEST(ForkMappingHoles, AdjacentDontForkIntervals) { RunScenario(Scenario::Adjacent); }
TEST(ForkMappingHoles, DontForkPrefix) { RunScenario(Scenario::Prefix); }
TEST(ForkMappingHoles, DontForkSuffix) { RunScenario(Scenario::Suffix); }
TEST(ForkMappingHoles, DoForkRestoresInheritance) { RunScenario(Scenario::DoFork); }
TEST(ForkMappingHoles, HintUsesExcludedRange) { RunScenario(Scenario::Hint); }
TEST(ForkMappingHoles, NoReplaceUsesExcludedRange) { RunScenario(Scenario::NoReplace); }
TEST(ForkMappingHoles, FailedFixedMappingPreservesFreeRange) { RunScenario(Scenario::FixedFailure); }
TEST(ForkMappingHoles, GrandchildRetainsFreeRange) { RunScenario(Scenario::Grandchild); }
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
