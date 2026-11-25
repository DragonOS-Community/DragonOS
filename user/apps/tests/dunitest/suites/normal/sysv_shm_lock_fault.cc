#include <gtest/gtest.h>

#include <atomic>
#include <pthread.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/shm.h>

namespace {

constexpr size_t kPageSize = 4096;
constexpr size_t kSegmentSize = 16 * kPageSize;
constexpr int kFaultThreads = 3;
constexpr int kRounds = 50000;

std::atomic<bool> g_stop_faults{false};

void* FaultSegment(void* arg) {
    auto* addr = static_cast<char*>(arg);
    while (!g_stop_faults.load()) {
        for (size_t offset = 0; offset < kSegmentSize; offset += kPageSize) {
            addr[offset] = 1;
        }
        madvise(addr, kSegmentSize, MADV_DONTNEED);
    }
    return nullptr;
}

void RunWhileSegmentFaults(void (*op)(int id)) {
    int id = shmget(IPC_PRIVATE, kSegmentSize, IPC_CREAT | 0600);
    ASSERT_GE(id, 0);
    auto* addr = static_cast<char*>(shmat(id, nullptr, 0));
    ASSERT_NE(reinterpret_cast<char*>(-1), addr);

    g_stop_faults.store(false);
    pthread_t threads[kFaultThreads];
    for (auto& thread : threads) {
        ASSERT_EQ(0, pthread_create(&thread, nullptr, FaultSegment, addr));
    }
    for (int round = 0; round < kRounds; round++) {
        op(id);
    }
    g_stop_faults.store(true);
    for (auto thread : threads) {
        pthread_join(thread, nullptr);
    }

    shmdt(addr);
    shmctl(id, IPC_RMID, nullptr);
}

TEST(SysvShmLockFault, LockUnlockWhileSegmentFaults) {
    RunWhileSegmentFaults([](int id) {
        EXPECT_EQ(0, shmctl(id, SHM_LOCK, nullptr));
        EXPECT_EQ(0, shmctl(id, SHM_UNLOCK, nullptr));
    });
}

TEST(SysvShmLockFault, ShmInfoWhileSegmentFaults) {
    RunWhileSegmentFaults([](int) {
        struct shm_info info = {};
        EXPECT_GE(shmctl(0, SHM_INFO, reinterpret_cast<struct shmid_ds*>(&info)), 0);
    });
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
