#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
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

TEST(EpollCtlWaitConcurrency, DeletedItemCannotBeRequeuedByConcurrentWakeup) {
    constexpr int kRounds = 500;
    constexpr int kRunning = 1;
    constexpr int kPaused = 2;
    constexpr int kStopped = 3;

    const int source = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    ASSERT_GE(source, 0) << strerror(errno);
    const int epfd = epoll_create1(EPOLL_CLOEXEC);
    ASSERT_GE(epfd, 0) << strerror(errno);

    std::atomic<int> state{kPaused};
    std::atomic<bool> paused{true};
    std::atomic<int> first_error{0};
    std::thread producer([&]() {
        const uint64_t one = 1;
        for (;;) {
            const int current = state.load(std::memory_order_acquire);
            if (current == kStopped) {
                return;
            }
            if (current == kPaused) {
                paused.store(true, std::memory_order_release);
                std::this_thread::yield();
                continue;
            }
            paused.store(false, std::memory_order_release);
            if (write(source, &one, sizeof(one)) != static_cast<ssize_t>(sizeof(one)) &&
                errno != EAGAIN &&
                errno != EWOULDBLOCK && errno != EINTR) {
                record_first_error(&first_error, errno != 0 ? errno : EIO);
                paused.store(true, std::memory_order_release);
                return;
            }
        }
    });

    for (int round = 0; round < kRounds && first_error.load() == 0; ++round) {
        epoll_event registration = {};
        registration.events = EPOLLIN;
        registration.data.u64 = kCtlSource;
        if (epoll_ctl(epfd, EPOLL_CTL_ADD, source, &registration) != 0) {
            record_first_error(&first_error, errno != 0 ? errno : EIO);
            break;
        }

        state.store(kRunning, std::memory_order_release);
        while (paused.load(std::memory_order_acquire) && first_error.load() == 0) {
            std::this_thread::yield();
        }
        if (first_error.load() != 0) {
            break;
        }
        if (epoll_ctl(epfd, EPOLL_CTL_DEL, source, nullptr) != 0) {
            record_first_error(&first_error, errno != 0 ? errno : EIO);
            break;
        }
        state.store(kPaused, std::memory_order_release);
        while (!paused.load(std::memory_order_acquire) && first_error.load() == 0) {
            std::this_thread::yield();
        }
        if (first_error.load() != 0) {
            break;
        }

        epoll_event observed = {};
        const int ready = epoll_wait(epfd, &observed, 1, 0);
        if (ready != 0) {
            record_first_error(&first_error, ready < 0 && errno != 0 ? errno : EIO);
            break;
        }

        uint64_t count = 0;
        while (read(source, &count, sizeof(count)) == sizeof(count)) {
        }
        if (errno != EAGAIN && errno != EWOULDBLOCK) {
            record_first_error(&first_error, errno != 0 ? errno : EIO);
            break;
        }
    }

    state.store(kStopped, std::memory_order_release);
    producer.join();
    EXPECT_EQ(0, first_error.load()) << strerror(first_error.load());
    close(epfd);
    close(source);
}

TEST(EpollCtlWaitConcurrency, DeletingOneDuplicatedSocketRegistrationKeepsTheOther) {
    int sockets[2] = {-1, -1};
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, sockets)) << strerror(errno);
    const int duplicate = dup(sockets[0]);
    ASSERT_GE(duplicate, 0) << strerror(errno);
    const int epfd = epoll_create1(EPOLL_CLOEXEC);
    ASSERT_GE(epfd, 0) << strerror(errno);

    epoll_event original_event = {};
    original_event.events = EPOLLIN;
    original_event.data.u64 = 1;
    ASSERT_EQ(0, epoll_ctl(epfd, EPOLL_CTL_ADD, sockets[0], &original_event)) << strerror(errno);

    epoll_event duplicate_event = {};
    duplicate_event.events = EPOLLIN;
    duplicate_event.data.u64 = 2;
    ASSERT_EQ(0, epoll_ctl(epfd, EPOLL_CTL_ADD, duplicate, &duplicate_event)) << strerror(errno);
    ASSERT_EQ(0, epoll_ctl(epfd, EPOLL_CTL_DEL, sockets[0], nullptr)) << strerror(errno);

    const char byte = 'x';
    ASSERT_EQ(1, write(sockets[1], &byte, sizeof(byte))) << strerror(errno);
    epoll_event observed = {};
    ASSERT_EQ(1, epoll_wait(epfd, &observed, 1, 1000)) << strerror(errno);
    EXPECT_EQ(2u, observed.data.u64);

    close(epfd);
    close(duplicate);
    close(sockets[0]);
    close(sockets[1]);
}

TEST(EpollCtlWaitConcurrency, ReusedFdNumberKeepsRegistrationsSeparatedByOpenFile) {
    int sockets[2] = {-1, -1};
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, sockets)) << strerror(errno);
    const int original_fd = sockets[0];
    const int keepalive = dup(original_fd);
    ASSERT_GE(keepalive, 0) << strerror(errno);
    const int epfd = epoll_create1(EPOLL_CLOEXEC);
    ASSERT_GE(epfd, 0) << strerror(errno);

    epoll_event old_event = {};
    old_event.events = EPOLLIN;
    old_event.data.u64 = 11;
    ASSERT_EQ(0, epoll_ctl(epfd, EPOLL_CTL_ADD, original_fd, &old_event)) << strerror(errno);
    ASSERT_EQ(0, close(original_fd)) << strerror(errno);

    const int replacement = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    ASSERT_EQ(original_fd, replacement) << "the replacement must reuse the registered fd number";
    epoll_event new_event = {};
    new_event.events = EPOLLIN;
    new_event.data.u64 = 22;
    ASSERT_EQ(0, epoll_ctl(epfd, EPOLL_CTL_ADD, replacement, &new_event)) << strerror(errno);
    ASSERT_EQ(0, epoll_ctl(epfd, EPOLL_CTL_DEL, replacement, nullptr)) << strerror(errno);

    const char byte = 'x';
    ASSERT_EQ(1, write(sockets[1], &byte, sizeof(byte))) << strerror(errno);
    epoll_event observed = {};
    ASSERT_EQ(1, epoll_wait(epfd, &observed, 1, 1000)) << strerror(errno);
    EXPECT_EQ(11u, observed.data.u64);

    close(replacement);
    close(epfd);
    close(keepalive);
    close(sockets[1]);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
