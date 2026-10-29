// Linux syslog shared-consumer and /proc/kmsg regression coverage.
#include <gtest/gtest.h>
#include <fcntl.h>
#include <poll.h>
#include <sys/mount.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <signal.h>
#include <pthread.h>
#include <atomic>
#include <thread>
#include <chrono>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>
#include <cerrno>
#include <cctype>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>
namespace {
int Log(int op, void* ptr = nullptr, int size = 0) {
    return syscall(SYS_syslog, op, ptr, size);
}
class SyslogStream : public testing::Test {
 protected:
    void SetUp() override {
        alarm(20);
        char path[] = "/tmp/syslog-stream-XXXXXX";
        char* made = mkdtemp(path);
        ASSERT_NE(nullptr, made);
        directory = made;
        int policy = open("/proc/sys/kernel/printk", O_RDONLY | O_CLOEXEC);
        ASSERT_GE(policy, 0);
        char text[128];
        ssize_t size = read(policy, text, sizeof(text));
        EXPECT_EQ(0, close(policy));
        ASSERT_GT(size, 0);
        console_policy.assign(text, size);
        // No test injection API: use the existing invalid-tmpfs diagnostic.
    }
    void TearDown() override {
        if (restore_console) {
            int policy = open("/proc/sys/kernel/printk", O_WRONLY | O_CLOEXEC);
            EXPECT_GE(policy, 0);
            if (policy >= 0) {
                EXPECT_EQ(static_cast<ssize_t>(console_policy.size()),
                          write(policy, console_policy.data(), console_policy.size()));
                EXPECT_EQ(0, close(policy));
            }
        }
        if (!directory.empty()) rmdir(directory.c_str());
        alarm(0);
    }
    void Emit() {
        ASSERT_EQ(-1, mount("tmpfs", directory.c_str(), "tmpfs", 0,
                            "size=not-a-number"));
        ASSERT_EQ(EINVAL, errno);
    }
    void Drain() {
        std::vector<char> bytes(1024 * 1024);
        for (int turn = 0; turn < 1024; ++turn) {
            int available = Log(9);
            ASSERT_GE(available, 0) << strerror(errno);
            if (!available) return;
            ASSERT_GT(Log(2, bytes.data(), bytes.size()), 0);
        }
        FAIL() << "global log consumer does not drain";
    }
    void ResetStream() {
        ASSERT_NO_FATAL_FAILURE(Drain());
        ASSERT_EQ(0, Log(5));
    }
    std::string Snapshot() {
        std::vector<char> bytes(1024 * 1024);
        int count = Log(3, bytes.data(), bytes.size());
        EXPECT_GE(count, 0);
        if (count < 0) return {};
        return std::string(bytes.data(), count);
    }
    void ExpectRemaining(const std::string& history, size_t consumed) {
        ASSERT_LE(consumed, history.size());
        const std::string expected = history.substr(consumed);
        std::vector<char> bytes(expected.size() + 1);
        int count = Log(2, bytes.data(), expected.size());
        ASSERT_GE(count, 0);
        ASSERT_EQ(expected.size(), static_cast<size_t>(count));
        EXPECT_EQ(expected, std::string(bytes.data(), count));
    }
    std::string directory, console_policy;
    bool restore_console = false;
};
TEST_F(SyslogStream, FixedCapacityDoesNotGrowAfterNewRecord) {
    int before = Log(10);
    ASSERT_GT(before, 0);
    Emit();
    EXPECT_EQ(before, Log(10));
}
TEST_F(SyslogStream, SizeUnreadExistsAndReadConsumesBytes) {
    ASSERT_NO_FATAL_FAILURE(Drain());
    Emit();
    int before = Log(9);
    ASSERT_GT(before, 1);
    char one;
    ASSERT_EQ(1, Log(2, &one, 1));
    EXPECT_EQ('<', one);
    EXPECT_EQ(before - 1, Log(9));
}
TEST_F(SyslogStream, ProcSharesTheSyslogCursor) {
    ASSERT_NO_FATAL_FAILURE(Drain());
    Emit();
    int before = Log(9);
    ASSERT_GT(before, 1);
    int fd = open("/proc/kmsg", O_RDONLY | O_NONBLOCK);
    ASSERT_GE(fd, 0);
    char one;
    int result = read(fd, &one, 1);
    close(fd);
    ASSERT_EQ(1, result);
    EXPECT_EQ(before - 1, Log(9));
}
TEST_F(SyslogStream, ClearDoesNotDiscardUnreadStream) {
    ASSERT_NO_FATAL_FAILURE(Drain());
    Emit();
    int before = Log(9);
    ASSERT_GT(before, 0);
    ASSERT_EQ(0, Log(5));
    EXPECT_EQ(before, Log(9));
}
TEST_F(SyslogStream, EmptyNonblockingProcReturnsAgain) {
    ASSERT_NO_FATAL_FAILURE(Drain());
    int fd = open("/proc/kmsg", O_RDONLY | O_NONBLOCK);
    ASSERT_GE(fd, 0);
    char one;
    errno = 0;
    int result = read(fd, &one, 1), saved = errno;
    close(fd);
    EXPECT_EQ(-1, result);
    EXPECT_EQ(EAGAIN, saved);
}
TEST_F(SyslogStream, EmptyProcPollIsNotReadable) {
    ASSERT_NO_FATAL_FAILURE(Drain());
    int fd = open("/proc/kmsg", O_RDONLY | O_NONBLOCK);
    ASSERT_GE(fd, 0);
    pollfd request{fd, POLLIN, 0};
    int result = poll(&request, 1, 0);
    close(fd);
    EXPECT_EQ(0, result);
    EXPECT_EQ(0, request.revents);
}
TEST_F(SyslogStream, ProcModeMatchesPrivilegedConsumer) {
    struct stat metadata{};
    ASSERT_EQ(0, stat("/proc/kmsg", &metadata));
    EXPECT_EQ(0400u, metadata.st_mode & 0777u);
}
TEST_F(SyslogStream, SnapshotUsesNumericPriority) {
    Emit();
    std::vector<char> bytes(1024 * 1024);
    int size = Log(3, bytes.data(), bytes.size());
    ASSERT_GE(size, 3);
    EXPECT_EQ('<', bytes[0]);
    EXPECT_TRUE(std::isdigit(static_cast<unsigned char>(bytes[1])));
    EXPECT_EQ('>', bytes[2]);
}

TEST_F(SyslogStream, FirstRecordCanBePartialButLaterRecordMustFit) {
    ASSERT_NO_FATAL_FAILURE(Drain());
    Emit();
    int first = Log(9);
    ASSERT_GT(first, 2);
    Emit();
    ASSERT_GT(Log(9), first);
    std::vector<char> bytes(first + 1);
    ASSERT_EQ(first, Log(2, bytes.data(), bytes.size()));
    int second = Log(9);
    ASSERT_GT(second, 2);
    ASSERT_EQ(1, Log(2, bytes.data(), 1));
    EXPECT_EQ('<', bytes[0]);
    EXPECT_EQ(second - 1, Log(9));
}
TEST_F(SyslogStream, ReadFaultConsumesItsReservedFragment) {
    ASSERT_NO_FATAL_FAILURE(ResetStream());
    Emit();
    int before = Log(9);
    ASSERT_GT(before, 1);
    void* page = mmap(nullptr, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, page);
    errno = 0;
    int result = Log(2, page, 1), saved = errno;
    EXPECT_EQ(0, munmap(page, 4096));
    EXPECT_EQ(-1, result);
    EXPECT_EQ(EFAULT, saved);
    ASSERT_NO_FATAL_FAILURE(ExpectRemaining(Snapshot(), 1));
}
TEST_F(SyslogStream, ProcSetFlagsAndDupKeepSharedConsumer) {
    ASSERT_NO_FATAL_FAILURE(Drain());
    int fd = open("/proc/kmsg", O_RDONLY);
    ASSERT_GE(fd, 0);
    int duplicate = dup(fd);
    ASSERT_GE(duplicate, 0);
    ASSERT_EQ(0, fcntl(duplicate, F_SETFL, O_NONBLOCK));
    char byte;
    errno = 0;
    EXPECT_EQ(-1, read(fd, &byte, 1));
    EXPECT_EQ(EAGAIN, errno);
    Emit();
    int before = Log(9);
    EXPECT_EQ(1, read(duplicate, &byte, 1));
    EXPECT_EQ(before - 1, Log(9));
    EXPECT_EQ(0, close(duplicate));
    EXPECT_EQ(0, close(fd));
}
TEST_F(SyslogStream, EpollReportsNewRecordsAndClearsAfterConsumption) {
    ASSERT_NO_FATAL_FAILURE(Drain());
    int fd = open("/proc/kmsg", O_RDONLY | O_NONBLOCK);
    ASSERT_GE(fd, 0);
    int ep = epoll_create1(EPOLL_CLOEXEC);
    ASSERT_GE(ep, 0);
    epoll_event event{};
    event.events = EPOLLIN;
    event.data.u64 = 123;
    ASSERT_EQ(0, epoll_ctl(ep, EPOLL_CTL_ADD, fd, &event));
    EXPECT_EQ(0, epoll_wait(ep, &event, 1, 0));
    Emit();
    ASSERT_EQ(1, epoll_wait(ep, &event, 1, 1000));
    EXPECT_EQ(123u, event.data.u64);
    EXPECT_NE(0u, event.events & EPOLLIN);
    ASSERT_NO_FATAL_FAILURE(Drain());
    EXPECT_EQ(0, epoll_wait(ep, &event, 1, 0));
    EXPECT_EQ(0, close(ep));
    EXPECT_EQ(0, close(fd));
}
TEST_F(SyslogStream, BlockingReadWakesForNewRecord) {
    ASSERT_NO_FATAL_FAILURE(Drain());
    int fd = open("/proc/kmsg", O_RDONLY);
    ASSERT_GE(fd, 0);
    std::atomic<bool> started{false}, finished{false};
    ssize_t count = -1;
    char byte = 0;
    std::thread reader([&] {
        started = true;
        count = read(fd, &byte, 1);
        finished = true;
    });
    while (!started.load()) std::this_thread::yield();
    std::this_thread::sleep_for(std::chrono::milliseconds(20));
    EXPECT_FALSE(finished.load());
    Emit();
    reader.join();
    EXPECT_EQ(1, count);
    EXPECT_EQ('<', byte);
    EXPECT_EQ(0, close(fd));
}
TEST_F(SyslogStream, OverwrittenCursorResumesAtRetainedOldestRecord) {
    ASSERT_NO_FATAL_FAILURE(Drain());
    Emit();
    char byte;
    ASSERT_EQ(1, Log(2, &byte, 1));
    // Discover descriptor/text saturation rather than baking a slot count
    // into the consumer ABI test. Keep the diagnostic workload bounded.
    int previous = Log(9), stable = 0;
    for (int index = 0; index < 32768 && stable < 4; ++index) {
        Emit();
        int available = Log(9);
        ASSERT_GT(available, 0);
        stable = available <= previous ? stable + 1 : 0;
        previous = available;
    }
    ASSERT_EQ(4, stable) << "log did not reach a retained-history boundary";
    ASSERT_GT(Log(9), 0);
    ASSERT_EQ(1, Log(2, &byte, 1));
    EXPECT_EQ('<', byte);
    ASSERT_NO_FATAL_FAILURE(Drain());
    EXPECT_EQ(0, Log(9));
}
TEST_F(SyslogStream, ConsoleControlsDoNotConsumeOrFilterRing) {
    restore_console = true;
    ASSERT_NO_FATAL_FAILURE(Drain());
    EXPECT_EQ(0, Log(6));
    EXPECT_EQ(0, Log(6));
    Emit();
    int unread = Log(9);
    EXPECT_GT(unread, 0);
    EXPECT_EQ(0, Log(7));
    EXPECT_EQ(unread, Log(9));
    EXPECT_EQ(0, Log(8, nullptr, 1));
    EXPECT_EQ(unread, Log(9));
    EXPECT_EQ(0, Log(8, nullptr, 7));
    errno = 0;
    EXPECT_EQ(-1, Log(8, nullptr, 0));
    EXPECT_EQ(EINVAL, errno);
}
TEST_F(SyslogStream, UnprivilegedChildCannotOpenPrivilegedConsumer) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (setgid(65534) || setuid(65534)) _exit(1);
        if (Log(9) != -1 || errno != EPERM) _exit(2);
        int fd = open("/proc/kmsg", O_RDONLY | O_NONBLOCK);
        if (fd >= 0) { close(fd); _exit(3); }
        if (errno != EACCES && errno != EPERM) _exit(4);
        if (Log(10) <= 0) _exit(5);
        _exit(0);
    }
    int status;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST_F(SyslogStream, LaterReadFaultReturnsEarlierSuccessfullyCopiedRecord) {
    ASSERT_NO_FATAL_FAILURE(ResetStream());
    Emit();
    int first = Log(9);
    ASSERT_GT(first, 0);
    ASSERT_LT(first, 4096);
    Emit();
    int total = Log(9);
    ASSERT_GT(total, first);
    ASSERT_LT(total, 4096);
    std::string before = Snapshot();
    ASSERT_EQ(static_cast<size_t>(total), before.size());
    // Each invalid mount emits a single-line warning, followed by any
    // syscall-return DEBUG. Only the faulting warning record is reserved.
    size_t fault_end = before.find('\n', first);
    ASSERT_NE(std::string::npos, fault_end);
    ASSERT_NE(std::string::npos, before.substr(first, fault_end + 1 - first)
                                  .find("Failed to produce filesystem"));
    ++fault_end;
    char* pages = static_cast<char*>(mmap(nullptr, 8192, PROT_READ | PROT_WRITE,
                                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, pages);
    ASSERT_EQ(0, mprotect(pages + 4096, 4096, PROT_NONE));
    EXPECT_EQ(first, Log(2, pages + 4096 - first, total));
    EXPECT_EQ(before.substr(0, first), std::string(pages + 4096 - first, first));
    EXPECT_EQ(0, munmap(pages, 8192));
    std::string after = Snapshot();
    ASSERT_GE(after.size(), before.size());
    ASSERT_EQ(before, after.substr(0, before.size()));
    ASSERT_NO_FATAL_FAILURE(ExpectRemaining(after, fault_end));
}
TEST_F(SyslogStream, OpenProcDescriptorRemainsUsableAfterPrivilegeDrop) {
    ASSERT_NO_FATAL_FAILURE(ResetStream());
    Emit();
    int before = Log(9);
    ASSERT_GT(before, 0);
    int fd = open("/proc/kmsg", O_RDONLY | O_NONBLOCK);
    ASSERT_GE(fd, 0);
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (setgid(65534) || setuid(65534)) _exit(1);
        char byte;
        _exit(read(fd, &byte, 1) == 1 && byte == '<' ? 0 : 2);
    }
    int status;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    EXPECT_EQ(0, close(fd));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    ASSERT_NO_FATAL_FAILURE(ExpectRemaining(Snapshot(), 1));
}

TEST_F(SyslogStream, EmptyNonblockingProcChecksBeforeZeroLength) {
    ASSERT_NO_FATAL_FAILURE(Drain());
    int fd = open("/proc/kmsg", O_RDONLY | O_NONBLOCK);
    ASSERT_GE(fd, 0);
    char byte;
    errno = 0;
    EXPECT_EQ(-1, read(fd, &byte, 0));
    EXPECT_EQ(EAGAIN, errno);
    EXPECT_EQ(0, close(fd));
}
void InterruptReader(int) {}
TEST_F(SyslogStream, BlockingReadIsInterruptible) {
    ASSERT_NO_FATAL_FAILURE(Drain());
    struct sigaction action{}, previous{};
    action.sa_handler = InterruptReader;
    sigemptyset(&action.sa_mask);
    ASSERT_EQ(0, sigaction(SIGUSR1, &action, &previous));
    int fd = open("/proc/kmsg", O_RDONLY);
    ASSERT_GE(fd, 0);
    std::atomic<bool> started{false}, finished{false};
    ssize_t count = 0;
    int error = 0;
    std::thread reader([&] {
        char byte;
        started = true;
        count = read(fd, &byte, 1);
        error = errno;
        finished = true;
    });
    while (!started.load()) std::this_thread::yield();
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
    while (!finished.load() && std::chrono::steady_clock::now() < deadline) {
        // The first signal can precede read's wait registration. Repeated
        // delivery proves interruption without assuming a scheduling delay.
        pthread_kill(reader.native_handle(), SIGUSR1);
        std::this_thread::sleep_for(std::chrono::milliseconds(1));
    }
    bool interrupted = finished.load();
    if (!interrupted) Emit();  // Join safely before reporting a failed assertion.
    reader.join();
    EXPECT_TRUE(interrupted);
    EXPECT_EQ(-1, count);
    EXPECT_EQ(EINTR, error);
    EXPECT_EQ(0, close(fd));
    EXPECT_EQ(0, sigaction(SIGUSR1, &previous, nullptr));
}

TEST_F(SyslogStream, ConcurrentFileAndEpollCloseKeepConsumerUsable) {
    for (int round = 0; round < 128; ++round) {
        int fd = open("/proc/kmsg", O_RDONLY | O_NONBLOCK);
        ASSERT_GE(fd, 0);
        int ep = epoll_create1(EPOLL_CLOEXEC);
        ASSERT_GE(ep, 0);
        epoll_event event{};
        event.events = EPOLLIN;
        ASSERT_EQ(0, epoll_ctl(ep, EPOLL_CTL_ADD, fd, &event));
        std::atomic<bool> go{false};
        int source_close = -1, epoll_close = -1;
        std::thread source([&] {
            while (!go.load()) std::this_thread::yield();
            source_close = close(fd);
        });
        std::thread poller([&] {
            while (!go.load()) std::this_thread::yield();
            epoll_close = close(ep);
        });
        go = true;
        source.join();
        poller.join();
        ASSERT_EQ(0, source_close);
        ASSERT_EQ(0, epoll_close);
    }
    ASSERT_NO_FATAL_FAILURE(Drain());
    Emit();
    ASSERT_GT(Log(9), 0);
    ASSERT_NO_FATAL_FAILURE(Drain());
}

TEST_F(SyslogStream, ProcNullBuffersPreserveNonblockAndValidationOrder) {
    ASSERT_NO_FATAL_FAILURE(ResetStream());
    int fd = open("/proc/kmsg", O_RDONLY | O_NONBLOCK);
    ASSERT_GE(fd, 0);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_read, fd, nullptr, 1));
    EXPECT_EQ(EAGAIN, errno);
    Emit();
    int before = Log(9);
    ASSERT_GT(before, 0);
    for (size_t size : {0u, 1u}) {
        errno = 0;
        EXPECT_EQ(-1, syscall(SYS_read, fd, nullptr, size));
        EXPECT_EQ(EINVAL, errno);
    }
    ASSERT_NO_FATAL_FAILURE(ExpectRemaining(Snapshot(), 0));
    ASSERT_NO_FATAL_FAILURE(Drain());
    ASSERT_EQ(0, fcntl(fd, F_SETFL, 0));
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_read, fd, nullptr, 1));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(0, close(fd));
}
TEST_F(SyslogStream, ScalarReadvNullSegmentDoesNotReserveLogBytes) {
    ASSERT_NO_FATAL_FAILURE(ResetStream());
    Emit();
    int before = Log(9);
    ASSERT_GT(before, 2);
    int fd = open("/proc/kmsg", O_RDONLY | O_NONBLOCK);
    ASSERT_GE(fd, 0);
    iovec invalid{nullptr, 1};
    errno = 0;
    EXPECT_EQ(-1, readv(fd, &invalid, 1));
    EXPECT_EQ(EINVAL, errno);
    char byte;
    iovec partial[]{{&byte, 1}, {nullptr, 1}};
    EXPECT_EQ(1, readv(fd, partial, 2));
    EXPECT_EQ('<', byte);
    ASSERT_NO_FATAL_FAILURE(ExpectRemaining(Snapshot(), 1));
    EXPECT_EQ(0, close(fd));
}
} // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
