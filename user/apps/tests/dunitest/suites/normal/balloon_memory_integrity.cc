#include <gtest/gtest.h>

#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#include <cerrno>
#include <cstdint>
#include <cstdio>
#include <cstdlib>

namespace {
constexpr size_t kMiB = 1024 * 1024;

uint64_t Pattern(size_t index) {
    return (static_cast<uint64_t>(index) * UINT64_C(0x9e3779b97f4a7c15)) ^
           UINT64_C(0xd09a60babb1e9876);
}

class Mapping {
 public:
    explicit Mapping(size_t bytes)
        : bytes_(bytes), address_(mmap(nullptr, bytes, PROT_READ | PROT_WRITE,
                                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0)) {}
    ~Mapping() {
        if (address_ != MAP_FAILED) munmap(address_, bytes_);
    }
    void* address() const { return address_; }
 private:
    size_t bytes_;
    void* address_;
};
}  // namespace

// During host QMP cycles run with DUNITEST_BALLOON_SECONDS=60. Every word,
// including page interiors, is checked: a misplaced host discard cannot hide
// behind touching only each page's first byte. No balloon is required for CI.
TEST(BalloonMemoryIntegrity, AllocatedContentsSurviveReclamation) {
    long seconds = 0;
    if (const char* value = getenv("DUNITEST_BALLOON_SECONDS")) {
        char* end = nullptr;
        errno = 0;
        seconds = strtol(value, &end, 10);
        ASSERT_EQ(0, errno);
        ASSERT_NE(value, end);
        ASSERT_EQ('\0', *end);
        ASSERT_GE(seconds, 0);
        ASSERT_LE(seconds, 300);
    }
    long mib = 32;
    if (const char* value = getenv("DUNITEST_BALLOON_MIB")) {
        char* end = nullptr;
        errno = 0;
        mib = strtol(value, &end, 10);
        ASSERT_EQ(0, errno);
        ASSERT_NE(value, end);
        ASSERT_EQ('\0', *end);
        ASSERT_GE(mib, 1);
        ASSERT_LE(mib, 512);
    }
    const size_t bytes = static_cast<size_t>(mib) * kMiB;
    Mapping mapping(bytes);
    ASSERT_NE(MAP_FAILED, mapping.address()) << "mmap errno=" << errno;
    auto* words = static_cast<volatile uint64_t*>(mapping.address());
    for (size_t i = 0; i < bytes / sizeof(uint64_t); ++i) words[i] = Pattern(i);
    timespec start;
    ASSERT_EQ(0, clock_gettime(CLOCK_MONOTONIC, &start));
    puts("balloon integrity ready");
    fflush(stdout);
    for (;;) {
        for (size_t i = 0; i < bytes / sizeof(uint64_t); ++i) {
            if (words[i] != Pattern(i)) {
                FAIL() << "allocated memory discarded/corrupted at byte "
                       << i * sizeof(uint64_t);
            }
        }
        timespec now;
        ASSERT_EQ(0, clock_gettime(CLOCK_MONOTONIC, &now));
        const auto elapsed_seconds = now.tv_sec - start.tv_sec;
        if (elapsed_seconds > seconds ||
            (elapsed_seconds == seconds && now.tv_nsec >= start.tv_nsec)) break;
        timespec delay{0, 20000000};
        while (nanosleep(&delay, &delay) < 0) ASSERT_EQ(EINTR, errno);
    }
}

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
