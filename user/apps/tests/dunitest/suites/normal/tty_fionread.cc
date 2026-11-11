#include <gtest/gtest.h>

#include <atomic>
#include <errno.h>
#include <pthread.h>
#include <pty.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

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

TEST(TtyFionread, ResultStoreFaultsWhileAddressSpaceIsContended) {
    int master = -1;
    int slave = -1;
    ASSERT_EQ(0, openpty(&master, &slave, nullptr, nullptr, nullptr))
        << "openpty failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_EQ(4, write(master, "abc\n", 4));

    pthread_t churn;
    ASSERT_EQ(0, pthread_create(&churn, nullptr, ChurnAddressSpace, nullptr));

    for (int round = 0; round < kRounds; round++) {
        auto* result = static_cast<int*>(mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE,
                                              MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
        ASSERT_NE(MAP_FAILED, result);
        ASSERT_EQ(0, ioctl(slave, FIONREAD, result))
            << "round " << round << ": errno=" << errno << " (" << strerror(errno) << ")";
        EXPECT_EQ(4, *result);
        munmap(result, kPageSize);
    }

    g_stop_churn.store(true);
    pthread_join(churn, nullptr);
    close(slave);
    close(master);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
