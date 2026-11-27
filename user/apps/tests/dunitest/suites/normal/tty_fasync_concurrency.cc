#include <gtest/gtest.h>

#include <cerrno>
#include <cstdlib>
#include <fcntl.h>
#include <pthread.h>
#include <pty.h>
#include <unistd.h>

namespace {

constexpr int kThreads = 8;
constexpr int kRounds = 20000;

class TtyFasyncConcurrency : public ::testing::Test {
protected:
    void SetUp() override {
        char name[128] = {};
        ASSERT_EQ(0, openpty(&master_, &slave_, name, nullptr, nullptr));
        for (auto& worker : workers_) {
            worker.fd = open(name, O_RDWR | O_NOCTTY);
            ASSERT_GE(worker.fd, 0) << "opening " << name;
        }
    }

    void TearDown() override {
        for (auto& worker : workers_) {
            if (worker.started) {
                const int error = pthread_join(worker.thread, nullptr);
                if (error != 0) {
                    // An unjoined worker may still reference fixture storage.
                    ADD_FAILURE() << "pthread_join failed with error " << error;
                    std::abort();
                }
                EXPECT_EQ(0, worker.error) << "F_SETFL failed on fd " << worker.fd;
            }
        }
        for (const auto& worker : workers_) {
            if (worker.fd >= 0) {
                EXPECT_EQ(0, close(worker.fd));
            }
        }
        if (slave_ >= 0) {
            EXPECT_EQ(0, close(slave_));
        }
        if (master_ >= 0) {
            EXPECT_EQ(0, close(master_));
        }
    }

    void StartWorkers() {
        for (auto& worker : workers_) {
            ASSERT_EQ(0, pthread_create(&worker.thread, nullptr, ToggleAsync, &worker));
            worker.started = true;
        }
    }

private:
    struct Worker {
        int fd = -1;
        pthread_t thread{};
        bool started = false;
        int error = 0;
    };

    static void* ToggleAsync(void* arg) {
        auto& worker = *static_cast<Worker*>(arg);
        for (int round = 0; round < kRounds; ++round) {
            if (fcntl(worker.fd, F_SETFL, O_ASYNC) != 0 ||
                fcntl(worker.fd, F_SETFL, 0) != 0) {
                worker.error = errno;
                break;
            }
        }
        return nullptr;
    }

    int master_ = -1;
    int slave_ = -1;
    Worker workers_[kThreads];
};

TEST_F(TtyFasyncConcurrency, AsyncTogglesOnOneTtyFromManyThreads) {
    StartWorkers();
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
