#include <gtest/gtest.h>

#include <atomic>
#include <cerrno>
#include <cstdlib>
#include <pthread.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/shm.h>

namespace {

constexpr size_t kPageSize = 4096;
constexpr size_t kSegmentSize = 16 * kPageSize;
constexpr int kRounds = 50000;

class SysvShmLockFault : public ::testing::Test {
protected:
    void SetUp() override {
        id_ = shmget(IPC_PRIVATE, kSegmentSize, IPC_CREAT | 0600);
        ASSERT_GE(id_, 0);
        addr_ = static_cast<char*>(shmat(id_, nullptr, 0));
        ASSERT_NE(addr_, reinterpret_cast<char*>(-1));
        ASSERT_EQ(0, pthread_create(&worker_, nullptr, RunWorker, this));
        started_ = true;
    }

    void TearDown() override {
        if (started_) {
            stop_.store(true);
            int error = pthread_join(worker_, nullptr);
            if (error != 0) {
                // The worker may still reference this fixture. Process exit drops the mapping.
                ADD_FAILURE() << "pthread_join failed with error " << error;
                EXPECT_EQ(0, shmctl(id_, IPC_RMID, nullptr));
                std::abort();
            }
            EXPECT_EQ(0, worker_error_);
            EXPECT_TRUE(worked_);
        }
        if (addr_ != reinterpret_cast<char*>(-1)) {
            EXPECT_EQ(0, shmdt(addr_));
        }
        if (id_ >= 0) {
            EXPECT_EQ(0, shmctl(id_, IPC_RMID, nullptr));
        }
    }

    int id_ = -1;

private:
    static void* RunWorker(void* arg) {
        static_cast<SysvShmLockFault*>(arg)->AccessMapping();
        return nullptr;
    }

    void AccessMapping() {
        while (!stop_.load()) {
            for (size_t offset = 0; offset < kSegmentSize; offset += kPageSize) {
                addr_[offset] = 1;
            }
            if (madvise(addr_, kSegmentSize, MADV_DONTNEED) != 0) {
                worker_error_ = errno;
                return;
            }
            worked_ = true;
        }
    }

    char* addr_ = reinterpret_cast<char*>(-1);
    pthread_t worker_{};
    bool started_ = false;
    std::atomic<bool> stop_{false};
    // Only read after pthread_join synchronizes with the worker.
    int worker_error_ = 0;
    bool worked_ = false;
};

TEST_F(SysvShmLockFault, LockUnlockWhileSegmentFaults) {
    for (int round = 0; round < kRounds; ++round) {
        ASSERT_EQ(0, shmctl(id_, SHM_LOCK, nullptr));
        ASSERT_EQ(0, shmctl(id_, SHM_UNLOCK, nullptr));
    }
}

TEST_F(SysvShmLockFault, ShmInfoWhileSegmentFaults) {
    for (int round = 0; round < kRounds; ++round) {
        struct shm_info info = {};
        ASSERT_GE(shmctl(0, SHM_INFO, reinterpret_cast<struct shmid_ds*>(&info)), 0);
    }
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
