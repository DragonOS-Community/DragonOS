#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/epoll.h>
#include <unistd.h>

#include <atomic>
#include <thread>

namespace {

constexpr int kEventRounds = 200;
constexpr int kCtlRounds = 20000;
constexpr int kMinimumCtlRounds = 100;
constexpr int kWaitTimeoutMs = 1000;
constexpr uint64_t kStableSource = 1;
constexpr uint64_t kCtlSource = 2;

void record_first_error(std::atomic<int>* first_error, int error) {
    int expected = 0;
    first_error->compare_exchange_strong(expected, error);
}

}  // namespace

TEST(EpollCtlWaitConcurrency, StableSourceSurvivesConcurrentCtlUpdates) {
    int stable_pipe[2] = {-1, -1};
    int ctl_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(stable_pipe)) << strerror(errno);
    ASSERT_EQ(0, pipe(ctl_pipe)) << strerror(errno);

    const int epfd = epoll_create1(EPOLL_CLOEXEC);
    ASSERT_GE(epfd, 0) << strerror(errno);

    epoll_event stable_event = {};
    stable_event.events = EPOLLIN;
    stable_event.data.u64 = kStableSource;
    ASSERT_EQ(0, epoll_ctl(epfd, EPOLL_CTL_ADD, stable_pipe[0], &stable_event)) << strerror(errno);

    std::atomic<bool> start{false};
    std::atomic<bool> event_phase_started{false};
    std::atomic<bool> abort{false};
    std::atomic<int> consumed{0};
    std::atomic<int> completed_ctl_rounds{0};
    std::atomic<int> first_error{0};

    std::thread producer([&]() {
        while (!start.load(std::memory_order_acquire)) {
            std::this_thread::yield();
        }
        while (completed_ctl_rounds.load(std::memory_order_acquire) < kMinimumCtlRounds &&
               !abort.load()) {
            std::this_thread::yield();
        }
        event_phase_started.store(true, std::memory_order_release);
        for (int round = 0; round < kEventRounds && !abort.load(); ++round) {
            const char byte = 'x';
            if (write(stable_pipe[1], &byte, sizeof(byte)) != 1) {
                record_first_error(&first_error, errno != 0 ? errno : EIO);
                return;
            }
            while (consumed.load(std::memory_order_acquire) <= round && !abort.load()) {
                usleep(100);
            }
        }
    });

    std::thread controller([&]() {
        while (!start.load(std::memory_order_acquire)) {
            std::this_thread::yield();
        }
        epoll_event event = {};
        event.events = EPOLLIN;
        event.data.u64 = kCtlSource;
        for (int round = 0; round < kCtlRounds && !abort.load(); ++round) {
            if (epoll_ctl(epfd, EPOLL_CTL_ADD, ctl_pipe[0], &event) != 0) {
                record_first_error(&first_error, errno);
                return;
            }
            event.events = EPOLLIN | EPOLLET;
            if (epoll_ctl(epfd, EPOLL_CTL_MOD, ctl_pipe[0], &event) != 0) {
                record_first_error(&first_error, errno);
                return;
            }
            if (epoll_ctl(epfd, EPOLL_CTL_DEL, ctl_pipe[0], nullptr) != 0) {
                record_first_error(&first_error, errno);
                return;
            }
            const int completed = completed_ctl_rounds.fetch_add(1, std::memory_order_release) + 1;
            if (completed == kMinimumCtlRounds) {
                while (!event_phase_started.load(std::memory_order_acquire) && !abort.load()) {
                    std::this_thread::yield();
                }
            }
            event.events = EPOLLIN;
        }
    });

    start.store(true, std::memory_order_release);
    int wait_error = 0;
    for (int round = 0; round < kEventRounds; ++round) {
        epoll_event event = {};
        const int ready = epoll_wait(epfd, &event, 1, kWaitTimeoutMs);
        if (ready != 1 || event.data.u64 != kStableSource) {
            wait_error = ready < 0 ? errno : ETIMEDOUT;
            break;
        }

        char byte = 0;
        if (read(stable_pipe[0], &byte, sizeof(byte)) != 1) {
            wait_error = errno != 0 ? errno : EIO;
            break;
        }
        consumed.fetch_add(1, std::memory_order_release);
    }

    abort.store(true, std::memory_order_release);
    producer.join();
    controller.join();

    EXPECT_EQ(0, wait_error) << "epoll_wait failed or lost the stable source: "
                             << strerror(wait_error);
    EXPECT_EQ(0, first_error.load())
        << "producer or epoll_ctl failed: " << strerror(first_error.load());
    EXPECT_EQ(kEventRounds, consumed.load());
    EXPECT_GE(completed_ctl_rounds.load(), kMinimumCtlRounds);

    close(epfd);
    close(stable_pipe[0]);
    close(stable_pipe[1]);
    close(ctl_pipe[0]);
    close(ctl_pipe[1]);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
