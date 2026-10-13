#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <atomic>
#include <chrono>
#include <memory>
#include <thread>
#include <vector>

namespace {

using Clock = std::chrono::steady_clock;
constexpr uint64_t kSource = 1;
constexpr uint64_t kCleanup = 2;

class Pipe {
public:
    Pipe() { valid_ = pipe(fds_) == 0; }
    ~Pipe() {
        if (valid_) {
            close(fds_[0]);
            if (fds_[1] >= 0) {
                close(fds_[1]);
            }
        }
    }
    bool valid() const { return valid_; }
    int reader() const { return fds_[0]; }
    int writer() const { return fds_[1]; }
    bool close_writer() {
        const int fd = fds_[1];
        fds_[1] = -1;
        return close(fd) == 0;
    }
    bool fill() const {
        const int flags = fcntl(writer(), F_GETFL);
        if (flags < 0 || fcntl(writer(), F_SETFL, flags | O_NONBLOCK) != 0) {
            return false;
        }
        const char bytes[4096] = {};
        // Bound setup even if nonblocking pipe writes are broken.
        size_t written = 0;
        while (written < 16 * 1024 * 1024) {
            const ssize_t count = write(writer(), bytes, sizeof(bytes));
            if (count > 0) {
                written += static_cast<size_t>(count);
            } else if (count < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
                return written > 0;
            } else if (count < 0 && errno == EINTR) {
                continue;
            } else {
                return false;
            }
        }
        return false;
    }
    bool drain_once() const {
        char bytes[8192];
        return read(reader(), bytes, sizeof(bytes)) >= 4096;
    }
    bool notify() const {
        const char byte = 'x';
        return write(fds_[1], &byte, 1) == 1;
    }

private:
    int fds_[2] = {-1, -1};
    bool valid_ = false;
};

// The waiter does no sleeping work before epoll_wait. Once its published TID
// is in S state, it has entered that wait, not an arbitrary startup delay.
char ThreadState(pid_t tid) {
    if (tid <= 0) {
        return '?';
    }
    char path[96];
    snprintf(path, sizeof(path), "/proc/self/task/%d/stat", tid);
    FILE* file = fopen(path, "r");
    if (file == nullptr) {
        return '?';
    }
    char line[1024] = {};
    const bool read = fgets(line, sizeof(line), file) != nullptr;
    fclose(file);
    const char* end = read ? strrchr(line, ')') : nullptr;
    return end != nullptr && end[1] == ' ' ? end[2] : '?';
}

class Waiter {
public:
    explicit Waiter(int source, uint32_t flags) {
        epfd_ = epoll_create1(EPOLL_CLOEXEC);
        if (epfd_ < 0 || !cleanup_.valid()) {
            return;
        }
        epoll_event event = {};
        event.events = flags;
        event.data.u64 = kSource;
        if (epoll_ctl(epfd_, EPOLL_CTL_ADD, source, &event) != 0) {
            return;
        }
        event.events = EPOLLIN;
        event.data.u64 = kCleanup;
        valid_ = epoll_ctl(epfd_, EPOLL_CTL_ADD, cleanup_.reader(), &event) == 0;
    }
    ~Waiter() {
        if (thread_.joinable()) {
            // Each private pipe has at most one byte. Cleanup neither consumes
            // the shared source nor relies on close(epfd) waking a blocked wait.
            cleanup_.notify();
            thread_.join();
        }
        if (epfd_ >= 0) {
            close(epfd_);
        }
    }
    bool valid() const { return valid_; }
    int fd() const { return epfd_; }
    void start() {
        thread_ = std::thread([this]() {
            tid_.store(static_cast<pid_t>(syscall(SYS_gettid)));
            epoll_event events[2] = {};
            // A finite fallback ensures a broken cleanup/wakeup implementation
            // cannot hang the entire regression binary during destruction.
            int count;
            do {
                count = epoll_wait(epfd_, events, 2, 10000);
            } while (count < 0 && errno == EINTR);
            if (count < 0) {
                error_.store(errno);
            }
            bool source = false;
            for (int i = 0; i < count; ++i) {
                source |= events[i].data.u64 == kSource;
            }
            source_.store(source);
            done_.store(true);
        });
    }
    bool sleeping() const { return !done() && ThreadState(tid_.load()) == 'S'; }
    bool done() const { return done_.load(); }
    bool source() const { return source_.load(); }
    int error() const { return error_.load(); }

private:
    int epfd_ = -1;
    bool valid_ = false;
    Pipe cleanup_;
    std::thread thread_;
    std::atomic<pid_t> tid_{0};
    std::atomic<bool> done_{false};
    std::atomic<bool> source_{false};
    std::atomic<int> error_{0};
};

bool AllBlocked(const std::vector<std::unique_ptr<Waiter>>& waiters) {
    const auto deadline = Clock::now() + std::chrono::seconds(3);
    do {
        bool sleeping = true;
        for (const auto& waiter : waiters) {
            sleeping &= waiter->sleeping();
        }
        if (sleeping) {
            return true;
        }
        std::this_thread::yield();
    } while (Clock::now() < deadline);
    return false;
}

void CheckWakeCounts(const std::vector<std::unique_ptr<Waiter>>& waiters,
                     size_t exclusive_count, size_t ordinary_count,
                     size_t expected_exclusive = 1) {
    const auto deadline = Clock::now() + std::chrono::seconds(2);
    size_t exclusive = 0;
    size_t ordinary = 0;
    do {
        exclusive = ordinary = 0;
        for (size_t i = 0; i < waiters.size(); ++i) {
            if (waiters[i]->done() && waiters[i]->source()) {
                (i < exclusive_count ? exclusive : ordinary)++;
            }
        }
        if (exclusive >= expected_exclusive && ordinary == ordinary_count) {
            break;
        }
        std::this_thread::yield();
    } while (Clock::now() < deadline);

    // Observe losers remaining blocked, instead of clearing the shared pipe
    // and mistaking racing readers for exclusive wakeup arbitration.
    const auto observe_until = Clock::now() + std::chrono::milliseconds(200);
    do {
        std::this_thread::yield();
    } while (Clock::now() < observe_until);
    exclusive = ordinary = 0;
    for (size_t i = 0; i < waiters.size(); ++i) {
        const auto& waiter = waiters[i];
        EXPECT_EQ(0, waiter->error());
        if (waiter->done() && waiter->source()) {
            (i < exclusive_count ? exclusive : ordinary)++;
        } else {
            EXPECT_TRUE(waiter->sleeping()) << "waiter " << i << " is not blocked";
        }
    }
    // Linux pipe write/read grant one exclusive callback; pipe_release uses
    // wake_up_interruptible_all (NULL key) to broadcast last-end closure.
    EXPECT_EQ(expected_exclusive, exclusive);
    EXPECT_EQ(ordinary_count, ordinary);
}

enum class SourceChange { Readable, Writable, Hangup };

void CheckSharedSource(bool idle_exclusive, size_t ordinary_count, bool edge,
                       SourceChange change = SourceChange::Readable) {
    Pipe source;
    ASSERT_TRUE(source.valid());
    const bool writable = change == SourceChange::Writable;
    if (writable) {
        ASSERT_TRUE(source.fill()) << "could not fill nonblocking pipe to EAGAIN";
    }
    const int source_fd = writable ? source.writer() : source.reader();
    const uint32_t interest = writable ? EPOLLOUT : EPOLLIN;
    const uint32_t flags = interest | EPOLLEXCLUSIVE | (edge ? EPOLLET : 0);
    std::unique_ptr<Waiter> idle;
    if (idle_exclusive) {
        idle = std::make_unique<Waiter>(source_fd, flags);
        ASSERT_TRUE(idle->valid());
        // Registered first, but no epoll_wait: must not consume the quota.
    }
    std::vector<std::unique_ptr<Waiter>> waiters;
    // A read-only OFD subscribed to OUT must not share the writer source
    // queue or consume its exclusive quota. Put this invalid direction first.
    const size_t kExclusiveCount = writable ? 4 : 3;
    for (size_t i = 0; i < kExclusiveCount + ordinary_count; ++i) {
        waiters.push_back(std::make_unique<Waiter>(
            writable && i == 0 ? source.reader() : source_fd,
            i < kExclusiveCount ? flags : interest));
        ASSERT_TRUE(waiters.back()->valid());
    }
    // In particular all ordinary registrations are added AFTER exclusives.
    for (const auto& waiter : waiters) {
        waiter->start();
    }
    ASSERT_TRUE(AllBlocked(waiters)) << "could not confirm epoll waiters in /proc";
    if (writable) {
        ASSERT_TRUE(source.drain_once());
    } else if (change == SourceChange::Hangup) {
        ASSERT_TRUE(source.close_writer());
    } else {
        ASSERT_TRUE(source.notify());
    }
    CheckWakeCounts(waiters, kExclusiveCount, ordinary_count,
                    change == SourceChange::Hangup ? kExclusiveCount : 1);
    if (idle) {
        epoll_event event = {};
        ASSERT_EQ(1, epoll_wait(idle->fd(), &event, 1, 0));
        EXPECT_EQ(kSource, event.data.u64);
    }
}

}  // namespace

TEST(EpollExclusive, OneBlockedExclusivePerPipeWake) { CheckSharedSource(false, 0, false); }

TEST(EpollExclusive, OrdinaryRegistrationsAfterExclusivesAllWake) {
    CheckSharedSource(false, 3, false);
}

TEST(EpollExclusive, IdleExclusiveDoesNotConsumeWakeQuota) { CheckSharedSource(true, 2, false); }

TEST(EpollExclusive, EdgeTriggeredExclusiveWake) { CheckSharedSource(false, 0, true); }

TEST(EpollExclusive, WritableExclusiveWakeAfterFullPipeDrain) {
    CheckSharedSource(false, 2, false, SourceChange::Writable);
}

TEST(EpollExclusive, HangupBroadcastsToAllExclusiveWaiters) {
    CheckSharedSource(false, 2, false, SourceChange::Hangup);
}

TEST(EpollExclusive, OrdinaryNestedEpollPropagatesReadiness) {
    Pipe source;
    ASSERT_TRUE(source.valid());
    Waiter inner(source.reader(), EPOLLIN);
    ASSERT_TRUE(inner.valid());
    std::vector<std::unique_ptr<Waiter>> outer;
    outer.push_back(std::make_unique<Waiter>(inner.fd(), EPOLLIN));
    ASSERT_TRUE(outer.front()->valid());
    outer.front()->start();
    ASSERT_TRUE(AllBlocked(outer));
    ASSERT_TRUE(source.notify());
    const auto deadline = Clock::now() + std::chrono::seconds(2);
    while (!outer.front()->done() && Clock::now() < deadline) {
        std::this_thread::yield();
    }
    ASSERT_TRUE(outer.front()->done());
    EXPECT_EQ(0, outer.front()->error());
    EXPECT_TRUE(outer.front()->source());
    epoll_event event = {};
    ASSERT_EQ(1, epoll_wait(inner.fd(), &event, 1, 0));
    EXPECT_EQ(kSource, event.data.u64);
}

TEST(EpollExclusive, ExclusiveControlValidationAndDeleteReadd) {
    Pipe source;
    ASSERT_TRUE(source.valid());
    Waiter watcher(source.reader(), EPOLLIN | EPOLLEXCLUSIVE);
    ASSERT_TRUE(watcher.valid());
    epoll_event event = {};
    event.events = EPOLLIN;
    errno = 0;
    EXPECT_EQ(-1, epoll_ctl(watcher.fd(), EPOLL_CTL_MOD, source.reader(), &event));
    EXPECT_EQ(EINVAL, errno);
    event.events = EPOLLIN | EPOLLEXCLUSIVE;
    errno = 0;
    EXPECT_EQ(-1, epoll_ctl(watcher.fd(), EPOLL_CTL_MOD, source.reader(), &event));
    EXPECT_EQ(EINVAL, errno);
    ASSERT_TRUE(source.notify());
    epoll_event delivered = {};
    ASSERT_EQ(1, epoll_wait(watcher.fd(), &delivered, 1, 1000));
    EXPECT_EQ(kSource, delivered.data.u64) << "rejected MOD changed registration data";
    ASSERT_EQ(0, epoll_ctl(watcher.fd(), EPOLL_CTL_DEL, source.reader(), nullptr));
    event.events = EPOLLIN | EPOLLEXCLUSIVE | 0x8000;
    errno = 0;
    EXPECT_EQ(-1, epoll_ctl(watcher.fd(), EPOLL_CTL_ADD, source.reader(), &event));
    EXPECT_EQ(EINVAL, errno);
    event.events = EPOLLIN | EPOLLEXCLUSIVE;
    event.events |= EPOLLONESHOT;
    errno = 0;
    EXPECT_EQ(-1, epoll_ctl(watcher.fd(), EPOLL_CTL_ADD, source.reader(), &event));
    EXPECT_EQ(EINVAL, errno);
    event.events = EPOLLIN | EPOLLEXCLUSIVE;
    event.data.u64 = kSource;
    ASSERT_EQ(0, epoll_ctl(watcher.fd(), EPOLL_CTL_ADD, source.reader(), &event));
    ASSERT_TRUE(source.notify());
    event = {};
    ASSERT_EQ(1, epoll_wait(watcher.fd(), &event, 1, 1000));
    EXPECT_EQ(kSource, event.data.u64);
}

TEST(EpollExclusive, ExclusiveNestedEpollRejected) {
    Pipe source;
    ASSERT_TRUE(source.valid());
    Waiter inner(source.reader(), EPOLLIN);
    Waiter outer(source.reader(), EPOLLIN);
    ASSERT_TRUE(inner.valid());
    ASSERT_TRUE(outer.valid());
    epoll_event event = {};
    event.events = EPOLLIN | EPOLLEXCLUSIVE;
    errno = 0;
    EXPECT_EQ(-1, epoll_ctl(outer.fd(), EPOLL_CTL_ADD, inner.fd(), &event));
    EXPECT_EQ(EINVAL, errno);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
