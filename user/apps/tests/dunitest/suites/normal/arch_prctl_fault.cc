#include <gtest/gtest.h>

#include <atomic>
#include <pthread.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#if defined(__x86_64__)
#include <asm/prctl.h>

namespace {

constexpr size_t kPageSize = 4096;
constexpr size_t kChurnSize = 64 << 20;
constexpr int kRounds = 20000;

std::atomic<bool> g_stop_churn{false};

void* ChurnAddressSpace(void*) {
    while (!g_stop_churn.load()) {
        auto* area = static_cast<char*>(mmap(nullptr, kChurnSize, PROT_READ | PROT_WRITE,
                                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
        if (area == MAP_FAILED) {
            continue;
        }
        for (size_t offset = 0; offset < kChurnSize; offset += kPageSize) {
            area[offset] = 1;
        }
        munmap(area, kChurnSize);
    }
    return nullptr;
}

TEST(ArchPrctlFault, ResultStoreFaultsWhileAddressSpaceIsContended) {
    long expected = 0;
    ASSERT_EQ(0, syscall(SYS_arch_prctl, ARCH_GET_FS, &expected));

    pthread_t churn;
    ASSERT_EQ(0, pthread_create(&churn, nullptr, ChurnAddressSpace, nullptr));

    for (int round = 0; round < kRounds; round++) {
        auto* result = static_cast<long*>(mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE,
                                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
        ASSERT_NE(MAP_FAILED, result);
        ASSERT_EQ(0, syscall(SYS_arch_prctl, ARCH_GET_FS, result));
        EXPECT_EQ(expected, *result);
        munmap(result, kPageSize);
    }

    g_stop_churn.store(true);
    pthread_join(churn, nullptr);
}

}  // namespace
#endif

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
