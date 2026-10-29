// RLIMIT_DATA charges private writable virtual mappings, including untouched
// pages. Isolate each limit change so a failed case cannot poison gtest's heap.
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <gtest/gtest.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef MREMAP_DONTUNMAP
#define MREMAP_DONTUNMAP 4
#endif

namespace {
size_t Page() { return static_cast<size_t>(sysconf(_SC_PAGESIZE)); }

// Avoid stdio allocation while limits are lowered, including when inspecting
// the permissions left behind by a partially successful mprotect request.
template <class Visit>
bool VisitMaps(Visit visit) {
    int fd = open("/proc/self/maps", O_RDONLY | O_CLOEXEC);
    if (fd < 0) return false;
    char buffer[4096], line[1024];
    size_t used = 0;
    bool ok = true;
    for (;;) {
        ssize_t n = read(fd, buffer, sizeof(buffer));
        if (n < 0 && errno == EINTR) continue;
        if (n < 0) { ok = false; break; }
        if (!n) break;
        for (ssize_t i = 0; i < n; ++i) {
            if (buffer[i] == '\n') {
                line[used] = 0;
                unsigned long begin = 0, end = 0;
                char perm[5] = {};
                if (sscanf(line, "%lx-%lx %4s", &begin, &end, perm) == 3)
                    visit(begin, end, perm);
                used = 0;
            } else if (used + 1 < sizeof(line)) {
                line[used++] = buffer[i];
            }
        }
    }
    close(fd);
    return ok;
}

// Do not use VmData: the kernel currently reports ELF data there, while the
// limit applies to every private writable non-stack VMA. Identify the inherited
// main stack by a live address, independent of procfs's pathname rendering.
// NPTL worker stacks have no GROWSDOWN flag and count as DATA mappings on Linux.
size_t DataBytes() {
    int stack_marker = 0;
    uintptr_t stack = reinterpret_cast<uintptr_t>(&stack_marker);
    size_t total = 0;
    bool ok = VisitMaps([&](uintptr_t begin, uintptr_t end, const char* perm) {
        if (perm[1] == 'w' && perm[3] == 'p' && !(begin <= stack && stack < end))
            total += end - begin;
    });
    return ok ? total : SIZE_MAX;
}

bool Writable(void* address) {
    bool writable = false;
    uintptr_t target = reinterpret_cast<uintptr_t>(address);
    VisitMaps([&](uintptr_t begin, uintptr_t end, const char* perm) {
        if (begin <= target && target < end) writable = perm[1] == 'w';
    });
    return writable;
}

void Limit(size_t soft, size_t hard = RLIM_INFINITY) {
    struct rlimit previous{};
    ASSERT_EQ(getrlimit(RLIMIT_DATA, &previous), 0);
    struct rlimit limit{soft, hard == RLIM_INFINITY ? previous.rlim_max : hard};
    ASSERT_EQ(setrlimit(RLIMIT_DATA, &limit), 0);
}
void* Map(size_t pages, int prot = PROT_READ | PROT_WRITE,
          int flags = MAP_PRIVATE | MAP_ANONYMOUS) {
    return mmap(nullptr, pages * Page(), prot, flags, -1, 0);
}
void Denied(void* result) {
    EXPECT_EQ(result, MAP_FAILED);
    EXPECT_EQ(errno, ENOMEM);
}
void Isolated(void (*body)()) {
    fflush(nullptr);
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (!child) {
        body();
        fflush(nullptr);
        _exit(::testing::Test::HasFailure() ? 1 : 0);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
    EXPECT_EQ(WEXITSTATUS(status), 0);
}
#define DATA_TEST(name) void name(); TEST(RlimitData, name) { Isolated(name); } void name()

DATA_TEST(PrivateWritableAndRwxConsumeLimit) {
    size_t baseline = DataBytes();
    ASSERT_NE(baseline, SIZE_MAX);
    Limit(baseline + Page());
    void* first = Map(1, PROT_READ | PROT_WRITE | PROT_EXEC);
    ASSERT_NE(first, MAP_FAILED);
    Denied(Map(1));
    ASSERT_EQ(munmap(first, Page()), 0);
    ASSERT_NE(Map(1), MAP_FAILED);
}

DATA_TEST(ReadonlySharedAndGrowdownDoNotConsumeData) {
    size_t baseline = DataBytes();
    ASSERT_NE(baseline, SIZE_MAX);
    Limit(baseline);
    ASSERT_NE(Map(2, PROT_READ), MAP_FAILED);
    ASSERT_NE(Map(2, PROT_NONE), MAP_FAILED);
    ASSERT_NE(Map(2, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS), MAP_FAILED);
    ASSERT_NE(Map(2, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS | MAP_GROWSDOWN), MAP_FAILED);
    Denied(Map(1));
}

DATA_TEST(PrivateFileChargedAndSharedFileExcluded) {
    char path[] = "/tmp/rlimit-data-XXXXXX";
    int fd = mkstemp(path);
    ASSERT_GE(fd, 0);
    ASSERT_EQ(unlink(path), 0);
    ASSERT_EQ(ftruncate(fd, 2 * Page()), 0);
    size_t baseline = DataBytes();
    ASSERT_NE(baseline, SIZE_MAX);
    Limit(baseline + Page());
    ASSERT_NE(mmap(nullptr, Page(), PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0), MAP_FAILED);
    Denied(mmap(nullptr, Page(), PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0));
    ASSERT_NE(mmap(nullptr, 2 * Page(), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0), MAP_FAILED);
    close(fd);
}

DATA_TEST(ZeroSoftLimitUsesHardLimit) {
    size_t baseline = DataBytes();
    ASSERT_NE(baseline, SIZE_MAX);
    Limit(0, baseline + Page());
    ASSERT_NE(Map(1), MAP_FAILED);
    Denied(Map(1));
}

DATA_TEST(ZeroSoftAndHardRejectWritableMapping) {
    Limit(0, 0);
    Denied(Map(1));
    ASSERT_NE(Map(1, PROT_READ), MAP_FAILED);
}

DATA_TEST(FixedReplacementChargesAllCoveredPagesForAdmission) {
    void* target = Map(1, PROT_READ);
    ASSERT_NE(target, MAP_FAILED);
    size_t baseline = DataBytes();
    ASSERT_NE(baseline, SIZE_MAX);
    Limit(baseline);
    // Linux retries may_expand_vm with len minus ALL overlapping VMA pages,
    // even when the replaced VMA was not itself a DATA mapping.
    ASSERT_EQ(mmap(target, Page(), PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0), target);
    *static_cast<volatile char*>(target) = 42;
    Denied(mmap(target, Page(), PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0));
    EXPECT_EQ(*static_cast<volatile char*>(target), 42);
}

DATA_TEST(FixedFailureKeepsOriginalMapping) {
    void* target = Map(2, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS);
    ASSERT_NE(target, MAP_FAILED);
    *static_cast<volatile char*>(target) = 71;
    size_t baseline = DataBytes();
    ASSERT_NE(baseline, SIZE_MAX);
    Limit(baseline - Page());
    Denied(mmap(target, 2 * Page(), PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0));
    EXPECT_EQ(*static_cast<volatile char*>(target), 71);
}

DATA_TEST(MprotectWritableAdmissionAndDowngradeRefund) {
    void* area = Map(2, PROT_READ);
    ASSERT_NE(area, MAP_FAILED);
    size_t baseline = DataBytes();
    ASSERT_NE(baseline, SIZE_MAX);
    Limit(baseline + Page());
    ASSERT_EQ(mprotect(area, Page(), PROT_READ | PROT_WRITE), 0);
    errno = 0;
    EXPECT_EQ(mprotect(static_cast<char*>(area) + Page(), Page(), PROT_READ | PROT_WRITE), -1);
    EXPECT_EQ(errno, ENOMEM);
    ASSERT_EQ(mprotect(area, Page(), PROT_READ), 0);
    ASSERT_EQ(mprotect(static_cast<char*>(area) + Page(), Page(), PROT_READ | PROT_WRITE), 0);
}

DATA_TEST(MprotectAlreadyWritableAboveLimitCanChangeExecute) {
    void* area = Map(1);
    ASSERT_NE(area, MAP_FAILED);
    size_t baseline = DataBytes();
    ASSERT_NE(baseline, SIZE_MAX);
    Limit(baseline - Page());
    ASSERT_EQ(mprotect(area, Page(), PROT_READ | PROT_WRITE | PROT_EXEC), 0);
    *static_cast<volatile char*>(area) = 19;
}

DATA_TEST(MremapGrowthNeedsDataBudget) {
    void* area = Map(1);
    ASSERT_NE(area, MAP_FAILED);
    *static_cast<char*>(area) = 17;
    size_t baseline = DataBytes();
    ASSERT_NE(baseline, SIZE_MAX);
    Limit(baseline);
    Denied(mremap(area, Page(), 2 * Page(), MREMAP_MAYMOVE));
    EXPECT_EQ(*static_cast<char*>(area), 17);
    ASSERT_EQ(mprotect(area, Page(), PROT_READ), 0);
    ASSERT_NE(mremap(area, Page(), 2 * Page(), MREMAP_MAYMOVE), MAP_FAILED);
}

DATA_TEST(MremapDontunmapRequiresFullNewBudget) {
    void* area = Map(1);
    ASSERT_NE(area, MAP_FAILED);
    size_t baseline = DataBytes();
    ASSERT_NE(baseline, SIZE_MAX);
    Limit(baseline);
    Denied(mremap(area, Page(), Page(), MREMAP_MAYMOVE | MREMAP_DONTUNMAP));
}

DATA_TEST(SharedDuplicationDoesNotConsumeData) {
    void* area = Map(1, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS);
    ASSERT_NE(area, MAP_FAILED);
    Limit(0, 0);
    void* alias = mremap(area, 0, Page(), MREMAP_MAYMOVE);
    ASSERT_NE(alias, MAP_FAILED);
    *static_cast<volatile char*>(alias) = 83;
    EXPECT_EQ(*static_cast<volatile char*>(area), 83);
}

DATA_TEST(MremapFixedFailureRemovesDestinationBeforeLimitCheck) {
    void* source = Map(1);
    void* target = Map(2, PROT_READ);
    ASSERT_NE(source, MAP_FAILED);
    ASSERT_NE(target, MAP_FAILED);
    *static_cast<char*>(source) = 93;
    size_t baseline = DataBytes();
    ASSERT_NE(baseline, SIZE_MAX);
    Limit(baseline);
    Denied(mremap(source, Page(), 2 * Page(), MREMAP_MAYMOVE | MREMAP_FIXED, target));
    EXPECT_EQ(*static_cast<char*>(source), 93);
    unsigned char resident = 0;
    errno = 0;
    EXPECT_EQ(mincore(target, Page(), &resident), -1);
    EXPECT_EQ(errno, ENOMEM);
}
DATA_TEST(MprotectCommitsSuccessfulPrefixBeforeDataFailure) {
    char* area = static_cast<char*>(Map(3, PROT_NONE));
    ASSERT_NE(area, MAP_FAILED);
    // Different protections prevent these three VMAs from merging before the
    // request. Linux admits them in address order against cumulative DATA.
    ASSERT_EQ(mprotect(area + Page(), Page(), PROT_READ), 0);
    size_t baseline = DataBytes();
    ASSERT_NE(baseline, SIZE_MAX);
    Limit(baseline + 2 * Page());
    errno = 0;
    EXPECT_EQ(mprotect(area, 3 * Page(), PROT_READ | PROT_WRITE), -1);
    EXPECT_EQ(errno, ENOMEM);
    EXPECT_TRUE(Writable(area));
    EXPECT_TRUE(Writable(area + Page()));
    EXPECT_FALSE(Writable(area + 2 * Page()));
    Denied(Map(1));
}

DATA_TEST(MprotectCommitsPrefixBeforeUnmappedHole) {
    char* area = static_cast<char*>(Map(3, PROT_READ));
    ASSERT_NE(area, MAP_FAILED);
    ASSERT_EQ(munmap(area + Page(), Page()), 0);
    size_t baseline = DataBytes();
    ASSERT_NE(baseline, SIZE_MAX);
    Limit(baseline + 3 * Page());
    errno = 0;
    EXPECT_EQ(mprotect(area, 3 * Page(), PROT_READ | PROT_WRITE), -1);
    EXPECT_EQ(errno, ENOMEM);
    EXPECT_TRUE(Writable(area));
    EXPECT_FALSE(Writable(area + 2 * Page()));
}

DATA_TEST(MremapShrinkRefundsDataBudget) {
    void* area = Map(2);
    ASSERT_NE(area, MAP_FAILED);
    size_t baseline = DataBytes();
    ASSERT_NE(baseline, SIZE_MAX);
    Limit(baseline);
    ASSERT_EQ(mremap(area, 2 * Page(), Page(), 0), area);
    ASSERT_NE(Map(1), MAP_FAILED);
    Denied(Map(1));
}

DATA_TEST(MremapDontunmapChargesBothMappings) {
    void* area = Map(1);
    ASSERT_NE(area, MAP_FAILED);
    size_t baseline = DataBytes();
    ASSERT_NE(baseline, SIZE_MAX);
    Limit(baseline + Page());
    ASSERT_NE(mremap(area, Page(), Page(), MREMAP_MAYMOVE | MREMAP_DONTUNMAP), MAP_FAILED);
    Denied(Map(1));
    ASSERT_EQ(munmap(area, Page()), 0);
    ASSERT_NE(Map(1), MAP_FAILED);
}

DATA_TEST(ForkInheritsPrivateWritableCharges) {
    size_t baseline = DataBytes();
    ASSERT_NE(baseline, SIZE_MAX);
    Limit(baseline + Page());
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (!child) {
        void* first = Map(1);
        if (first == MAP_FAILED) _exit(1);
        errno = 0;
        if (Map(1) != MAP_FAILED || errno != ENOMEM) _exit(2);
        if (munmap(first, Page())) _exit(3);
        if (Map(1) == MAP_FAILED) _exit(4);
        _exit(0);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
    // The fork child's admission did not consume the parent's budget.
    ASSERT_NE(Map(1), MAP_FAILED);
}

DATA_TEST(MprotectAllowsWriteWhenBothOldAndNewExceedAs) {
    void* area = Map(1, PROT_READ);
    ASSERT_NE(area, MAP_FAILED);
    size_t baseline = DataBytes();
    ASSERT_NE(baseline, SIZE_MAX);
    struct rlimit old_as{}, old_data{};
    ASSERT_EQ(getrlimit(RLIMIT_AS, &old_as), 0);
    ASSERT_EQ(getrlimit(RLIMIT_DATA, &old_data), 0);
    Limit(baseline);
    struct rlimit lowered_as{1, old_as.rlim_max};
    ASSERT_EQ(setrlimit(RLIMIT_AS, &lowered_as), 0);
    // Both may_expand_vm(old) and may_expand_vm(new) fail AS. Linux permits
    // this permission change rather than interpreting DATA as an isolated veto.
    int result = mprotect(area, Page(), PROT_READ | PROT_WRITE);
    int saved_errno = errno;
    int restored_as = setrlimit(RLIMIT_AS, &old_as);
    int restored_data = setrlimit(RLIMIT_DATA, &old_data);
    ASSERT_EQ(restored_as, 0);
    ASSERT_EQ(restored_data, 0);
    EXPECT_EQ(result, 0) << "errno=" << saved_errno;
    EXPECT_TRUE(Writable(area));
}

struct MmapRace {
    pthread_barrier_t barrier;
    int fd;
    void* result[2];
    int errors[2];
};
struct MmapWorker { MmapRace* race; unsigned index; };
void* RaceMap(void* pointer) {
    auto* worker = static_cast<MmapWorker*>(pointer);
    MmapRace& race = *worker->race;
    pthread_barrier_wait(&race.barrier);
    errno = 0;
    race.result[worker->index] = mmap(nullptr, Page(), PROT_READ | PROT_WRITE,
                                    MAP_PRIVATE, race.fd, 0);
    race.errors[worker->index] = errno;
    pthread_barrier_wait(&race.barrier);
    // Main restores the limit before threads exit or gtest formats diagnostics.
    pthread_barrier_wait(&race.barrier);
    return nullptr;
}

DATA_TEST(ConcurrentPrivateFileMappingsShareOneDataBudget) {
    MmapRace race{};
    char path[] = "/tmp/rlimit-data-race-XXXXXX";
    race.fd = mkstemp(path);
    ASSERT_GE(race.fd, 0);
    ASSERT_EQ(unlink(path), 0);
    ASSERT_EQ(ftruncate(race.fd, Page()), 0);
    ASSERT_EQ(pthread_barrier_init(&race.barrier, nullptr, 3), 0);
    MmapWorker workers[] = {{&race, 0}, {&race, 1}};
    pthread_t threads[2];
    // Create TLS, stacks and libc thread bookkeeping before measuring DATA.
    ASSERT_EQ(pthread_create(&threads[0], nullptr, RaceMap, &workers[0]), 0);
    ASSERT_EQ(pthread_create(&threads[1], nullptr, RaceMap, &workers[1]), 0);
    size_t baseline = DataBytes();
    ASSERT_NE(baseline, SIZE_MAX);
    struct rlimit previous{};
    ASSERT_EQ(getrlimit(RLIMIT_DATA, &previous), 0);
    Limit(baseline + Page());
    pthread_barrier_wait(&race.barrier);
    pthread_barrier_wait(&race.barrier);
    int restored = setrlimit(RLIMIT_DATA, &previous);
    pthread_barrier_wait(&race.barrier);
    ASSERT_EQ(restored, 0);
    ASSERT_EQ(pthread_join(threads[0], nullptr), 0);
    ASSERT_EQ(pthread_join(threads[1], nullptr), 0);
    unsigned successes = 0;
    for (unsigned i = 0; i < 2; ++i) {
        if (race.result[i] != MAP_FAILED) {
            ++successes;
            EXPECT_EQ(munmap(race.result[i], Page()), 0);
        } else {
            EXPECT_EQ(race.errors[i], ENOMEM);
        }
    }
    EXPECT_EQ(successes, 1u);
    EXPECT_EQ(pthread_barrier_destroy(&race.barrier), 0);
    close(race.fd);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
