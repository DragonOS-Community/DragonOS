#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <unistd.h>

#include <atomic>
#include <chrono>
#include <mutex>
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

TEST(EpollCtlWaitConcurrency, DoubleEpollOneShotExchangeMakesProgress) {
    constexpr int kRounds = 1 << 14;
    constexpr uint64_t kEventTag = 0x2202;
    constexpr auto kStallTimeout = std::chrono::seconds(10);
    constexpr char kRequest[] = "hello";
    constexpr char kStop[] = "world";
    static_assert(sizeof(kRequest) == sizeof(kStop));

    int sockets[2] = {-1, -1};
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, sockets)) << strerror(errno);
    const int epfds[2] = {epoll_create1(EPOLL_CLOEXEC), epoll_create1(EPOLL_CLOEXEC)};
    ASSERT_GE(epfds[0], 0) << strerror(errno);
    ASSERT_GE(epfds[1], 0) << strerror(errno);

    for (int epfd : epfds) {
        epoll_event event = {};
        event.events = EPOLLIN | EPOLLONESHOT;
        event.data.u64 = kEventTag;
        ASSERT_EQ(0, epoll_ctl(epfd, EPOLL_CTL_ADD, sockets[1], &event)) << strerror(errno);
    }

    std::atomic<int> first_error{0};
    std::atomic<int> completed_rounds{0};
    std::atomic<bool> finished{false};
    std::mutex socket_lock;

    auto write_exact = [&](int fd, const char* data, size_t size) {
        size_t done = 0;
        while (done < size) {
            const ssize_t written = send(fd, data + done, size - done, MSG_NOSIGNAL);
            if (written > 0) {
                done += static_cast<size_t>(written);
                continue;
            }
            if (written < 0 && errno == EINTR) {
                continue;
            }
            record_first_error(&first_error, errno != 0 ? errno : EIO);
            return false;
        }
        return true;
    };

    auto read_exact = [&](int fd, char* data, size_t size) {
        size_t done = 0;
        while (done < size) {
            const ssize_t received = read(fd, data + done, size - done);
            if (received > 0) {
                done += static_cast<size_t>(received);
                continue;
            }
            if (received < 0 && errno == EINTR) {
                continue;
            }
            record_first_error(&first_error,
                               received == 0 ? EPIPE : (errno != 0 ? errno : EIO));
            return false;
        }
        return true;
    };

    std::thread server([&]() {
        char request[sizeof(kRequest)] = {};
        for (int round = 0; round < kRounds && first_error.load() == 0; ++round) {
            if (!read_exact(sockets[0], request, sizeof(request))) {
                return;
            }
            if (memcmp(request, kRequest, sizeof(request)) != 0) {
                record_first_error(&first_error, EPROTO);
                return;
            }
            const char* response = round < kRounds - 2 ? kRequest : kStop;
            if (!write_exact(sockets[0], response, sizeof(kRequest))) {
                return;
            }
            completed_rounds.store(round + 1, std::memory_order_release);
        }
    });

    auto client = [&](int epfd) {
        bool rearm = false;
        char response[sizeof(kRequest)] = {};
        while (first_error.load() == 0) {
            if (rearm) {
                epoll_event event = {};
                event.events = EPOLLIN | EPOLLONESHOT;
                event.data.u64 = kEventTag;
                if (epoll_ctl(epfd, EPOLL_CTL_MOD, sockets[1], &event) != 0) {
                    record_first_error(&first_error, errno != 0 ? errno : EIO);
                    return;
                }
            }

            {
                std::lock_guard<std::mutex> lock(socket_lock);
                if (!write_exact(sockets[1], kRequest, sizeof(kRequest))) {
                    return;
                }
            }

            epoll_event event = {};
            int ready;
            do {
                ready = epoll_wait(epfd, &event, 1, -1);
            } while (ready < 0 && errno == EINTR);
            if (ready != 1 || event.data.u64 != kEventTag) {
                record_first_error(&first_error, ready < 0 && errno != 0 ? errno : EIO);
                return;
            }
            rearm = true;

            {
                std::lock_guard<std::mutex> lock(socket_lock);
                if (!read_exact(sockets[1], response, sizeof(response))) {
                    return;
                }
            }
            if (memcmp(response, kStop, sizeof(response)) == 0) {
                return;
            }
            if (memcmp(response, kRequest, sizeof(response)) != 0) {
                record_first_error(&first_error, EPROTO);
                return;
            }
        }
    };

    std::thread client1(client, epfds[0]);
    std::thread client2(client, epfds[1]);
    std::thread watchdog([&]() {
        int last_progress = completed_rounds.load(std::memory_order_acquire);
        auto stall_deadline = std::chrono::steady_clock::now() + kStallTimeout;
        while (!finished.load(std::memory_order_acquire)) {
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
            const int progress = completed_rounds.load(std::memory_order_acquire);
            if (progress != last_progress) {
                last_progress = progress;
                stall_deadline = std::chrono::steady_clock::now() + kStallTimeout;
            } else if (std::chrono::steady_clock::now() >= stall_deadline) {
                record_first_error(&first_error, ETIMEDOUT);
                shutdown(sockets[0], SHUT_RDWR);
                shutdown(sockets[1], SHUT_RDWR);
                return;
            }
        }
    });

    server.join();
    client1.join();
    client2.join();
    finished.store(true, std::memory_order_release);
    watchdog.join();

    EXPECT_EQ(0, first_error.load())
        << "double EPOLLONESHOT exchange stalled or failed: " << strerror(first_error.load());

    close(epfds[0]);
    close(epfds[1]);
    close(sockets[0]);
    close(sockets[1]);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
