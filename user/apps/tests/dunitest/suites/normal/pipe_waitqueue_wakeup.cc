#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <sys/socket.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <algorithm>
#include <chrono>
#include <cstdio>
#include <vector>

namespace {

volatile sig_atomic_t g_sigpipe_count = 0;
volatile sig_atomic_t g_sigio_count = 0;

void SigpipeHandler(int) {
    ++g_sigpipe_count;
}

void SigioHandler(int) {
    ++g_sigio_count;
}

void SleepForMillis(long millis) {
    timespec ts {};
    ts.tv_sec = millis / 1000;
    ts.tv_nsec = (millis % 1000) * 1000 * 1000;
    while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
    }
}

bool WaitForChild(pid_t child, int* status, int rounds = 300) {
    for (int i = 0; i < rounds; ++i) {
        pid_t ret = waitpid(child, status, WNOHANG);
        if (ret == child) {
            return true;
        }
        if (ret < 0 && errno != EINTR) {
            return false;
        }
        SleepForMillis(10);
    }
    return false;
}

bool WaitForSleepingProcess(pid_t pid) {
    char path[64] = {};
    std::snprintf(path, sizeof(path), "/proc/%d/stat", pid);
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(1);
    while (std::chrono::steady_clock::now() < deadline) {
        FILE* stat = std::fopen(path, "r");
        if (stat != nullptr) {
            char line[512] = {};
            const bool read = std::fgets(line, sizeof(line), stat) != nullptr;
            std::fclose(stat);
            if (read) {
                const char* comm_end = strrchr(line, ')');
                if (comm_end != nullptr && comm_end[1] == ' ' &&
                    (comm_end[2] == 'S' || comm_end[2] == 'D')) {
                    return true;
                }
            }
        }
        usleep(1'000);
    }
    return false;
}

bool WaitForSignalCount(volatile sig_atomic_t* count, sig_atomic_t expected) {
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(1);
    while (std::chrono::steady_clock::now() < deadline) {
        if (*count >= expected) {
            return true;
        }
        usleep(1'000);
    }
    return *count >= expected;
}

void KillAndReap(pid_t child) {
    if (child <= 0) {
        return;
    }
    kill(child, SIGKILL);
    while (waitpid(child, nullptr, 0) < 0 && errno == EINTR) {
    }
}

class ChildProcessGuard {
public:
    ChildProcessGuard() = default;
    explicit ChildProcessGuard(pid_t child) : child_(child) {}
    ChildProcessGuard(const ChildProcessGuard&) = delete;
    ChildProcessGuard& operator=(const ChildProcessGuard&) = delete;

    ~ChildProcessGuard() {
        Cleanup();
    }

    void Reset(pid_t child) {
        Cleanup();
        child_ = child;
    }

    void Release() {
        child_ = -1;
    }

    void Cleanup() {
        if (child_ > 0) {
            KillAndReap(child_);
            child_ = -1;
        }
    }

private:
    pid_t child_ = -1;
};

class ScopedSignalAction {
public:
    explicit ScopedSignalAction(int signal) : signal_(signal) {}
    ScopedSignalAction(const ScopedSignalAction&) = delete;
    ScopedSignalAction& operator=(const ScopedSignalAction&) = delete;

    ~ScopedSignalAction() {
        if (installed_) {
            sigaction(signal_, &old_action_, nullptr);
        }
    }

    bool Install(const struct sigaction& action) {
        installed_ = sigaction(signal_, &action, &old_action_) == 0;
        return installed_;
    }

private:
    int signal_;
    bool installed_ = false;
    struct sigaction old_action_ {};
};

bool WriteAll(int fd, const char* data, size_t len) {
    size_t written = 0;
    while (written < len) {
        const ssize_t n = write(fd, data + written, len - written);
        if (n > 0) {
            written += static_cast<size_t>(n);
            continue;
        }
        if (n < 0 && errno == EINTR) {
            continue;
        }
        return false;
    }
    return true;
}

bool ReadExactly(int fd, size_t len) {
    std::vector<char> bytes(4096);
    size_t read_bytes = 0;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
    while (read_bytes < len) {
        const auto now = std::chrono::steady_clock::now();
        if (now >= deadline) {
            return false;
        }
        const auto remaining =
            std::chrono::duration_cast<std::chrono::milliseconds>(deadline - now).count();
        pollfd wait_fd {};
        wait_fd.fd = fd;
        wait_fd.events = POLLIN;
        const int ready = poll(&wait_fd, 1, static_cast<int>(std::max<int64_t>(1, remaining)));
        if (ready < 0 && errno == EINTR) {
            continue;
        }
        if (ready <= 0) {
            return false;
        }
        const size_t chunk = std::min(bytes.size(), len - read_bytes);
        const ssize_t n = read(fd, bytes.data(), chunk);
        if (n > 0) {
            read_bytes += static_cast<size_t>(n);
            continue;
        }
        if (n < 0 && errno == EINTR) {
            continue;
        }
        return false;
    }
    return true;
}

bool ReadAllInto(int fd, char* data, size_t len) {
    size_t read_bytes = 0;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
    while (read_bytes < len) {
        const auto now = std::chrono::steady_clock::now();
        if (now >= deadline) {
            return false;
        }
        const auto remaining =
            std::chrono::duration_cast<std::chrono::milliseconds>(deadline - now).count();
        pollfd wait_fd {};
        wait_fd.fd = fd;
        wait_fd.events = POLLIN | POLLHUP;
        const int ready = poll(&wait_fd, 1, static_cast<int>(std::max<int64_t>(1, remaining)));
        if (ready < 0 && errno == EINTR) {
            continue;
        }
        if (ready <= 0) {
            return false;
        }
        const ssize_t n = read(fd, data + read_bytes, len - read_bytes);
        if (n > 0) {
            read_bytes += static_cast<size_t>(n);
            continue;
        }
        if (n < 0 && errno == EINTR) {
            continue;
        }
        return false;
    }
    return true;
}

bool FillPipeExactly(int write_fd, size_t capacity) {
    const int old_flags = fcntl(write_fd, F_GETFL);
    if (old_flags < 0 || fcntl(write_fd, F_SETFL, old_flags | O_NONBLOCK) != 0) {
        return false;
    }

    std::vector<char> bytes(4096, 'f');
    size_t written = 0;
    while (written < capacity) {
        const size_t chunk = std::min(bytes.size(), capacity - written);
        const ssize_t n = write(write_fd, bytes.data(), chunk);
        if (n > 0) {
            written += static_cast<size_t>(n);
            continue;
        }
        if (n < 0 && errno == EINTR) {
            continue;
        }
        fcntl(write_fd, F_SETFL, old_flags);
        return false;
    }
    return fcntl(write_fd, F_SETFL, old_flags) == 0;
}

}  // namespace

TEST(PipeWaitqueueWakeup, ZeroLengthIoHasNoEndpointSideEffects) {
    int fds[2] = {-1, -1};
    ASSERT_EQ(0, pipe(fds)) << strerror(errno);

    const int read_flags = fcntl(fds[0], F_GETFL);
    ASSERT_GE(read_flags, 0) << strerror(errno);
    ASSERT_EQ(0, fcntl(fds[0], F_SETFL, read_flags | O_NONBLOCK)) << strerror(errno);

    char byte = 0;
    errno = 0;
    EXPECT_EQ(0, read(fds[0], &byte, 0)) << strerror(errno);

    struct sigaction action {};
    action.sa_handler = SigpipeHandler;
    sigemptyset(&action.sa_mask);
    ScopedSignalAction sigpipe_guard(SIGPIPE);
    ASSERT_TRUE(sigpipe_guard.Install(action)) << strerror(errno);
    g_sigpipe_count = 0;

    ASSERT_EQ(0, close(fds[0])) << strerror(errno);
    errno = 0;
    EXPECT_EQ(0, write(fds[1], &byte, 0)) << strerror(errno);
    EXPECT_EQ(0, g_sigpipe_count);

    close(fds[1]);
}

TEST(PipeWaitqueueWakeup, NonblockingWriteWithoutReaderRaisesSigpipe) {
    int fds[2] = {-1, -1};
    ASSERT_EQ(0, pipe(fds)) << strerror(errno);

    const int flags = fcntl(fds[1], F_GETFL);
    ASSERT_GE(flags, 0) << strerror(errno);
    ASSERT_EQ(0, fcntl(fds[1], F_SETFL, flags | O_NONBLOCK)) << strerror(errno);

    struct sigaction action {};
    action.sa_handler = SigpipeHandler;
    sigemptyset(&action.sa_mask);
    ScopedSignalAction sigpipe_guard(SIGPIPE);
    ASSERT_TRUE(sigpipe_guard.Install(action)) << strerror(errno);
    g_sigpipe_count = 0;

    ASSERT_EQ(0, close(fds[0])) << strerror(errno);
    char byte = 'x';
    errno = 0;
    EXPECT_EQ(-1, write(fds[1], &byte, 1));
    EXPECT_EQ(EPIPE, errno);
    EXPECT_EQ(1, g_sigpipe_count);

    close(fds[1]);
}

TEST(PipeWaitqueueWakeup, NonblockingSocketSpliceWithoutReaderDoesNotConsumeInput) {
    int source[2] = {-1, -1};
    int destination[2] = {-1, -1};
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, source)) << strerror(errno);
    ASSERT_EQ(0, pipe(destination)) << strerror(errno);

    const char payload[] = "splice-input";
    ASSERT_TRUE(WriteAll(source[1], payload, sizeof(payload))) << strerror(errno);

    struct sigaction action {};
    action.sa_handler = SigpipeHandler;
    sigemptyset(&action.sa_mask);
    ScopedSignalAction sigpipe_guard(SIGPIPE);
    ASSERT_TRUE(sigpipe_guard.Install(action)) << strerror(errno);
    g_sigpipe_count = 0;

    ASSERT_EQ(0, close(destination[0])) << strerror(errno);
    destination[0] = -1;
    errno = 0;
    EXPECT_EQ(-1, splice(source[0], nullptr, destination[1], nullptr, sizeof(payload),
                         SPLICE_F_NONBLOCK));
    EXPECT_EQ(EPIPE, errno);
    EXPECT_EQ(1, g_sigpipe_count);

    char received[sizeof(payload)] = {};
    ASSERT_TRUE(ReadAllInto(source[0], received, sizeof(received))) << strerror(errno);
    EXPECT_EQ(0, memcmp(payload, received, sizeof(payload)))
        << "failed splice consumed stream input before reporting EPIPE";

    close(source[0]);
    close(source[1]);
    close(destination[1]);
}

TEST(PipeWaitqueueWakeup, EligibleWriterIsNotBlockedBehindLargerWriter) {
    int data[2] = {-1, -1};
    int ready_large[2] = {-1, -1};
    int ready_small[2] = {-1, -1};
    ASSERT_EQ(0, pipe(data)) << strerror(errno);
    ASSERT_EQ(0, pipe(ready_large)) << strerror(errno);
    ASSERT_EQ(0, pipe(ready_small)) << strerror(errno);
    ASSERT_EQ(4096, fcntl(data[1], F_SETPIPE_SZ, 4096)) << strerror(errno);
    const int capacity = fcntl(data[1], F_GETPIPE_SZ);
    ASSERT_EQ(4096, capacity) << strerror(errno);
    ASSERT_TRUE(FillPipeExactly(data[1], static_cast<size_t>(capacity))) << strerror(errno);

    pid_t large = fork();
    ASSERT_GE(large, 0) << strerror(errno);
    ChildProcessGuard large_guard(large);
    if (large == 0) {
        close(data[0]);
        close(ready_large[0]);
        close(ready_small[0]);
        close(ready_small[1]);
        const char ready = 'L';
        if (!WriteAll(ready_large[1], &ready, 1)) {
            _exit(2);
        }
        close(ready_large[1]);
        std::vector<char> payload(4096, 'L');
        _exit(WriteAll(data[1], payload.data(), payload.size()) ? 0 : 3);
    }

    close(ready_large[1]);
    char marker = 0;
    ASSERT_EQ(1, read(ready_large[0], &marker, 1)) << strerror(errno);
    ASSERT_EQ('L', marker);
    close(ready_large[0]);
    if (!WaitForSleepingProcess(large)) {
        large_guard.Cleanup();
        FAIL() << "large writer did not enter the pipe wait path";
    }

    pid_t small = fork();
    ASSERT_GE(small, 0) << strerror(errno);
    ChildProcessGuard small_guard(small);
    if (small == 0) {
        close(data[0]);
        close(ready_small[0]);
        const char ready = 'S';
        if (!WriteAll(ready_small[1], &ready, 1)) {
            _exit(4);
        }
        close(ready_small[1]);
        const char byte = 's';
        _exit(WriteAll(data[1], &byte, 1) ? 0 : 5);
    }

    close(ready_small[1]);
    marker = 0;
    ASSERT_EQ(1, read(ready_small[0], &marker, 1)) << strerror(errno);
    ASSERT_EQ('S', marker);
    close(ready_small[0]);
    if (!WaitForSleepingProcess(small)) {
        small_guard.Cleanup();
        large_guard.Cleanup();
        FAIL() << "small writer did not enter the pipe wait path";
    }

    char byte = 0;
    ASSERT_EQ(1, read(data[0], &byte, 1)) << strerror(errno);

    int small_status = 0;
    const bool small_finished = WaitForChild(small, &small_status, 100);
    if (!small_finished) {
        small_guard.Cleanup();
    } else {
        small_guard.Release();
    }

    int large_status = 0;
    const bool large_finished_early = waitpid(large, &large_status, WNOHANG) == large;
    if (!large_finished_early) {
        const size_t buffered = small_finished ? static_cast<size_t>(capacity)
                                               : static_cast<size_t>(capacity - 1);
        ASSERT_TRUE(ReadExactly(data[0], buffered)) << strerror(errno);
        if (!WaitForChild(large, &large_status)) {
            FAIL() << "large writer did not finish after sufficient space was released";
        }
        large_guard.Release();
    } else {
        large_guard.Release();
    }

    EXPECT_TRUE(small_finished) << "eligible 1-byte writer remained asleep behind 4096-byte writer";
    if (small_finished) {
        ASSERT_TRUE(WIFEXITED(small_status));
        EXPECT_EQ(0, WEXITSTATUS(small_status));
    }
    EXPECT_FALSE(large_finished_early);
    ASSERT_TRUE(WIFEXITED(large_status));
    EXPECT_EQ(0, WEXITSTATUS(large_status));

    close(data[0]);
    close(data[1]);
}

TEST(PipeWaitqueueWakeup, HomogeneousWritersPassBatonAfterPartialDrain) {
    int data[2] = {-1, -1};
    int ready[2] = {-1, -1};
    ASSERT_EQ(0, pipe(data)) << strerror(errno);
    ASSERT_EQ(0, pipe(ready)) << strerror(errno);
    ASSERT_EQ(4096, fcntl(data[1], F_SETPIPE_SZ, 4096)) << strerror(errno);
    ASSERT_TRUE(FillPipeExactly(data[1], 4096)) << strerror(errno);

    pid_t writers[2] = {-1, -1};
    ChildProcessGuard writer_guards[2];
    for (size_t i = 0; i < 2; ++i) {
        writers[i] = fork();
        ASSERT_GE(writers[i], 0) << strerror(errno);
        writer_guards[i].Reset(writers[i]);
        if (writers[i] == 0) {
            close(data[0]);
            close(ready[0]);
            const char marker = static_cast<char>('0' + i);
            if (!WriteAll(ready[1], &marker, 1)) {
                _exit(2);
            }
            close(ready[1]);
            const char byte = static_cast<char>('a' + i);
            _exit(write(data[1], &byte, 1) == 1 ? 0 : 3);
        }
    }

    close(ready[1]);
    ASSERT_TRUE(ReadExactly(ready[0], 2)) << strerror(errno);
    close(ready[0]);
    for (pid_t writer : writers) {
        if (!WaitForSleepingProcess(writer)) {
            writer_guards[0].Cleanup();
            writer_guards[1].Cleanup();
            FAIL() << "homogeneous writer did not enter the pipe wait path";
        }
    }

    char drained[2] = {};
    ASSERT_EQ(2, read(data[0], drained, sizeof(drained))) << strerror(errno);

    for (size_t i = 0; i < 2; ++i) {
        int status = 0;
        if (!WaitForChild(writers[i], &status)) {
            FAIL() << "writer baton did not advance every eligible writer";
        }
        writer_guards[i].Release();
        ASSERT_TRUE(WIFEXITED(status));
        EXPECT_EQ(0, WEXITSTATUS(status));
    }

    close(data[0]);
    close(data[1]);
}

TEST(PipeWaitqueueWakeup, PartialWriteNotifiesEpollBeforeWriterSleeps) {
    int data[2] = {-1, -1};
    int ready[2] = {-1, -1};
    ASSERT_EQ(0, pipe(data)) << strerror(errno);
    ASSERT_EQ(0, pipe(ready)) << strerror(errno);
    ASSERT_EQ(4096, fcntl(data[1], F_SETPIPE_SZ, 4096)) << strerror(errno);

    const int epfd = epoll_create1(0);
    ASSERT_GE(epfd, 0) << strerror(errno);
    epoll_event interest {};
    interest.events = EPOLLIN;
    interest.data.fd = data[0];
    ASSERT_EQ(0, epoll_ctl(epfd, EPOLL_CTL_ADD, data[0], &interest)) << strerror(errno);

    pid_t writer = fork();
    ASSERT_GE(writer, 0) << strerror(errno);
    ChildProcessGuard writer_guard(writer);
    if (writer == 0) {
        close(data[0]);
        close(ready[0]);
        const char marker = 'W';
        if (!WriteAll(ready[1], &marker, 1)) {
            _exit(2);
        }
        close(ready[1]);
        std::vector<char> payload(8192, 'w');
        _exit(WriteAll(data[1], payload.data(), payload.size()) ? 0 : 3);
    }

    close(data[1]);
    close(ready[1]);
    char marker = 0;
    ASSERT_EQ(1, read(ready[0], &marker, 1)) << strerror(errno);
    ASSERT_EQ('W', marker);
    close(ready[0]);
    if (!WaitForSleepingProcess(writer)) {
        writer_guard.Cleanup();
        FAIL() << "large writer did not block after publishing partial data";
    }

    epoll_event event {};
    const int ready_count = epoll_wait(epfd, &event, 1, 200);

    ASSERT_TRUE(ReadExactly(data[0], 4096)) << strerror(errno);
    int status = 0;
    if (!WaitForChild(writer, &status)) {
        FAIL() << "large writer did not finish after reader drained the pipe";
    }
    writer_guard.Release();

    EXPECT_EQ(1, ready_count) << strerror(errno);
    if (ready_count == 1) {
        EXPECT_NE(0U, event.events & EPOLLIN);
    }
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));

    close(epfd);
    close(data[0]);
}

TEST(PipeWaitqueueWakeup, PartialWritePublishesSigioAndReturnsPartialAfterReaderClose) {
    int data[2] = {-1, -1};
    int ready[2] = {-1, -1};
    ASSERT_EQ(0, pipe(data)) << strerror(errno);
    ASSERT_EQ(0, pipe(ready)) << strerror(errno);
    ASSERT_EQ(4096, fcntl(data[1], F_SETPIPE_SZ, 4096)) << strerror(errno);

    struct sigaction sigio_action {};
    sigio_action.sa_handler = SigioHandler;
    sigemptyset(&sigio_action.sa_mask);
    ScopedSignalAction sigio_guard(SIGIO);
    ASSERT_TRUE(sigio_guard.Install(sigio_action)) << strerror(errno);
    g_sigio_count = 0;
    ASSERT_EQ(0, fcntl(data[0], F_SETOWN, getpid())) << strerror(errno);
    const int read_flags = fcntl(data[0], F_GETFL);
    ASSERT_GE(read_flags, 0) << strerror(errno);
    ASSERT_EQ(0, fcntl(data[0], F_SETFL, read_flags | O_ASYNC)) << strerror(errno);

    pid_t writer = fork();
    ASSERT_GE(writer, 0) << strerror(errno);
    ChildProcessGuard writer_guard(writer);
    if (writer == 0) {
        close(data[0]);
        close(ready[0]);
        struct sigaction sigpipe_action {};
        sigpipe_action.sa_handler = SigpipeHandler;
        sigemptyset(&sigpipe_action.sa_mask);
        if (sigaction(SIGPIPE, &sigpipe_action, nullptr) != 0) {
            _exit(2);
        }
        g_sigpipe_count = 0;
        const char marker = 'P';
        if (!WriteAll(ready[1], &marker, 1)) {
            _exit(3);
        }
        close(ready[1]);
        std::vector<char> payload(8192, 'p');
        errno = 0;
        const ssize_t written = write(data[1], payload.data(), payload.size());
        _exit(written == 4096 && g_sigpipe_count == 1 ? 0 : 4);
    }

    close(data[1]);
    close(ready[1]);
    char marker = 0;
    ASSERT_EQ(1, read(ready[0], &marker, 1)) << strerror(errno);
    ASSERT_EQ('P', marker);
    close(ready[0]);
    if (!WaitForSleepingProcess(writer)) {
        writer_guard.Cleanup();
        FAIL() << "large writer did not block after partial progress";
    }
    EXPECT_TRUE(WaitForSignalCount(&g_sigio_count, 1))
        << "partial write did not publish read-side SIGIO before sleeping";

    ASSERT_EQ(0, close(data[0])) << strerror(errno);
    int status = 0;
    if (!WaitForChild(writer, &status)) {
        FAIL() << "writer did not return after the last reader closed";
    }
    writer_guard.Release();
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(PipeWaitqueueWakeup, PipeToPipeSpliceWakesSourceWriterAndDestinationReader) {
    int source[2] = {-1, -1};
    int destination[2] = {-1, -1};
    int ready[2] = {-1, -1};
    ASSERT_EQ(0, pipe(source)) << strerror(errno);
    ASSERT_EQ(0, pipe(destination)) << strerror(errno);
    ASSERT_EQ(0, pipe(ready)) << strerror(errno);
    ASSERT_EQ(4096, fcntl(source[1], F_SETPIPE_SZ, 4096)) << strerror(errno);
    ASSERT_EQ(4096, fcntl(destination[1], F_SETPIPE_SZ, 4096)) << strerror(errno);
    ASSERT_TRUE(FillPipeExactly(source[1], 4096)) << strerror(errno);

    pid_t writer = fork();
    ASSERT_GE(writer, 0) << strerror(errno);
    ChildProcessGuard writer_guard(writer);
    if (writer == 0) {
        close(source[0]);
        close(destination[0]);
        close(destination[1]);
        close(ready[0]);
        const char marker = 'W';
        if (!WriteAll(ready[1], &marker, 1)) {
            _exit(2);
        }
        const char byte = 'w';
        _exit(write(source[1], &byte, 1) == 1 ? 0 : 3);
    }

    pid_t reader = fork();
    ASSERT_GE(reader, 0) << strerror(errno);
    ChildProcessGuard reader_guard(reader);
    if (reader == 0) {
        close(destination[1]);
        close(source[0]);
        close(source[1]);
        close(ready[0]);
        const char marker = 'R';
        if (!WriteAll(ready[1], &marker, 1)) {
            _exit(4);
        }
        char byte = 0;
        _exit(read(destination[0], &byte, 1) == 1 ? 0 : 5);
    }

    close(ready[1]);
    ASSERT_TRUE(ReadExactly(ready[0], 2)) << strerror(errno);
    close(ready[0]);
    if (!WaitForSleepingProcess(writer) || !WaitForSleepingProcess(reader)) {
        writer_guard.Cleanup();
        reader_guard.Cleanup();
        FAIL() << "splice participants did not enter their pipe wait paths";
    }

    ASSERT_EQ(4096, splice(source[0], nullptr, destination[1], nullptr, 4096, 0))
        << strerror(errno);
    int writer_status = 0;
    int reader_status = 0;
    ASSERT_TRUE(WaitForChild(writer, &writer_status));
    writer_guard.Release();
    ASSERT_TRUE(WaitForChild(reader, &reader_status));
    reader_guard.Release();
    ASSERT_TRUE(WIFEXITED(writer_status));
    EXPECT_EQ(0, WEXITSTATUS(writer_status));
    ASSERT_TRUE(WIFEXITED(reader_status));
    EXPECT_EQ(0, WEXITSTATUS(reader_status));

    close(source[0]);
    close(source[1]);
    close(destination[0]);
    close(destination[1]);
}

TEST(PipeWaitqueueWakeup, BlockingSocketSpliceKeepsObservedPipeSpace) {
    int source[2] = {-1, -1};
    int destination[2] = {-1, -1};
    int splice_ready[2] = {-1, -1};
    int writer_ready[2] = {-1, -1};
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, source)) << strerror(errno);
    ASSERT_EQ(0, pipe(destination)) << strerror(errno);
    ASSERT_EQ(0, pipe(splice_ready)) << strerror(errno);
    ASSERT_EQ(0, pipe(writer_ready)) << strerror(errno);
    ASSERT_EQ(4096, fcntl(destination[1], F_SETPIPE_SZ, 4096)) << strerror(errno);
    ASSERT_TRUE(FillPipeExactly(destination[1], 4096)) << strerror(errno);

    pid_t splice_worker = fork();
    ASSERT_GE(splice_worker, 0) << strerror(errno);
    ChildProcessGuard splice_guard(splice_worker);
    if (splice_worker == 0) {
        close(source[1]);
        close(destination[0]);
        close(splice_ready[0]);
        close(writer_ready[0]);
        close(writer_ready[1]);
        const char marker = 'S';
        if (!WriteAll(splice_ready[1], &marker, 1)) {
            _exit(2);
        }
        close(splice_ready[1]);
        const ssize_t copied =
            splice(source[0], nullptr, destination[1], nullptr, 4096, 0);
        _exit(copied == 4096 ? 0 : 3);
    }

    close(splice_ready[1]);
    char marker = 0;
    ASSERT_EQ(1, read(splice_ready[0], &marker, 1)) << strerror(errno);
    ASSERT_EQ('S', marker);
    close(splice_ready[0]);
    if (!WaitForSleepingProcess(splice_worker)) {
        FAIL() << "splice worker did not wait for destination space";
    }

    pid_t writer = fork();
    ASSERT_GE(writer, 0) << strerror(errno);
    ChildProcessGuard writer_guard(writer);
    if (writer == 0) {
        close(source[0]);
        close(source[1]);
        close(destination[0]);
        close(splice_ready[0]);
        close(splice_ready[1]);
        close(writer_ready[0]);
        const char ready = 'W';
        if (!WriteAll(writer_ready[1], &ready, 1)) {
            _exit(4);
        }
        close(writer_ready[1]);
        const char byte = 'w';
        _exit(write(destination[1], &byte, 1) == 1 ? 0 : 5);
    }

    close(writer_ready[1]);
    marker = 0;
    ASSERT_EQ(1, read(writer_ready[0], &marker, 1)) << strerror(errno);
    ASSERT_EQ('W', marker);
    close(writer_ready[0]);
    if (!WaitForSleepingProcess(writer)) {
        FAIL() << "competing writer did not wait for destination space";
    }

    std::vector<char> drained(4096);
    ASSERT_TRUE(ReadAllInto(destination[0], drained.data(), drained.size())) << strerror(errno);

    int writer_status = 0;
    if (WaitForChild(writer, &writer_status, 20)) {
        writer_guard.Release();
        FAIL() << "competing writer stole space owned by the blocked splice";
    }

    std::vector<char> payload(4096, 's');
    ASSERT_TRUE(WriteAll(source[1], payload.data(), payload.size())) << strerror(errno);

    int splice_status = 0;
    if (!WaitForChild(splice_worker, &splice_status)) {
        FAIL() << "splice did not commit after input became readable";
    }
    splice_guard.Release();
    ASSERT_TRUE(WIFEXITED(splice_status));
    ASSERT_EQ(0, WEXITSTATUS(splice_status));

    std::vector<char> copied(4096);
    ASSERT_TRUE(ReadAllInto(destination[0], copied.data(), copied.size())) << strerror(errno);
    EXPECT_EQ(payload, copied);

    if (!WaitForChild(writer, &writer_status)) {
        FAIL() << "competing writer did not proceed after splice data was drained";
    }
    writer_guard.Release();
    ASSERT_TRUE(WIFEXITED(writer_status));
    ASSERT_EQ(0, WEXITSTATUS(writer_status));
    char writer_byte = 0;
    ASSERT_EQ(1, read(destination[0], &writer_byte, 1)) << strerror(errno);
    EXPECT_EQ('w', writer_byte);

    close(source[0]);
    close(source[1]);
    close(destination[0]);
    close(destination[1]);
}

TEST(PipeWaitqueueWakeup, TeeReturnsPartialWithoutConsumingSource) {
    int source[2] = {-1, -1};
    int destination[2] = {-1, -1};
    int ready[2] = {-1, -1};
    int release_reader[2] = {-1, -1};
    int tee_result[2] = {-1, -1};
    ASSERT_EQ(0, pipe(source)) << strerror(errno);
    ASSERT_EQ(0, pipe(destination)) << strerror(errno);
    ASSERT_EQ(0, pipe(ready)) << strerror(errno);
    ASSERT_EQ(0, pipe(release_reader)) << strerror(errno);
    ASSERT_EQ(0, pipe(tee_result)) << strerror(errno);
    ASSERT_EQ(8192, fcntl(source[1], F_SETPIPE_SZ, 8192)) << strerror(errno);
    ASSERT_EQ(4096, fcntl(destination[1], F_SETPIPE_SZ, 4096)) << strerror(errno);
    std::vector<char> payload(8192, 't');
    ASSERT_TRUE(WriteAll(source[1], payload.data(), payload.size())) << strerror(errno);

    pid_t reader = fork();
    ASSERT_GE(reader, 0) << strerror(errno);
    ChildProcessGuard reader_guard(reader);
    if (reader == 0) {
        close(source[0]);
        close(source[1]);
        close(destination[1]);
        close(ready[0]);
        close(release_reader[1]);
        close(tee_result[0]);
        close(tee_result[1]);
        const char marker = 'T';
        if (!WriteAll(ready[1], &marker, 1)) {
            _exit(2);
        }
        close(ready[1]);
        char byte = 0;
        if (read(destination[0], &byte, 1) != 1) {
            _exit(3);
        }
        _exit(read(release_reader[0], &byte, 1) == 1 ? 0 : 4);
    }

    close(destination[0]);
    close(ready[1]);
    close(release_reader[0]);
    char marker = 0;
    ASSERT_EQ(1, read(ready[0], &marker, 1)) << strerror(errno);
    ASSERT_EQ('T', marker);
    close(ready[0]);
    if (!WaitForSleepingProcess(reader)) {
        reader_guard.Cleanup();
        FAIL() << "tee destination reader did not block";
    }

    struct TeeResult {
        ssize_t copied;
        int error;
    };
    pid_t tee_worker = fork();
    ASSERT_GE(tee_worker, 0) << strerror(errno);
    ChildProcessGuard tee_worker_guard(tee_worker);
    if (tee_worker == 0) {
        close(tee_result[0]);
        errno = 0;
        TeeResult result {};
        result.copied = tee(source[0], destination[1], payload.size(), 0);
        result.error = errno;
        const bool reported = WriteAll(tee_result[1], reinterpret_cast<const char*>(&result),
                                       sizeof(result));
        _exit(reported ? 0 : 5);
    }

    close(tee_result[1]);
    int tee_status = 0;
    if (!WaitForChild(tee_worker, &tee_status)) {
        FAIL() << "blocking tee did not return after making partial progress";
    }
    tee_worker_guard.Release();
    ASSERT_TRUE(WIFEXITED(tee_status));
    ASSERT_EQ(0, WEXITSTATUS(tee_status));
    TeeResult result {};
    ASSERT_EQ(static_cast<ssize_t>(sizeof(result)),
              read(tee_result[0], &result, sizeof(result)))
        << strerror(errno);
    close(tee_result[0]);
    errno = result.error;
    const ssize_t copied = result.copied;
    EXPECT_GT(copied, 0);
    EXPECT_LT(copied, static_cast<ssize_t>(payload.size()));

    const char release = 'R';
    ASSERT_TRUE(WriteAll(release_reader[1], &release, 1)) << strerror(errno);
    close(release_reader[1]);
    int reader_status = 0;
    ASSERT_TRUE(WaitForChild(reader, &reader_status));
    reader_guard.Release();
    ASSERT_TRUE(WIFEXITED(reader_status));
    EXPECT_EQ(0, WEXITSTATUS(reader_status));
    ASSERT_TRUE(ReadExactly(source[0], payload.size()))
        << "tee unexpectedly consumed source data";

    close(source[0]);
    close(source[1]);
    close(destination[1]);
}

TEST(PipeWaitqueueWakeup, BlockedReadersPassBatonForRemainingData) {
    int data[2] = {-1, -1};
    int ready[2] = {-1, -1};
    ASSERT_EQ(0, pipe(data)) << strerror(errno);
    ASSERT_EQ(0, pipe(ready)) << strerror(errno);

    pid_t readers[2] = {-1, -1};
    ChildProcessGuard reader_guards[2];
    for (size_t i = 0; i < 2; ++i) {
        readers[i] = fork();
        ASSERT_GE(readers[i], 0) << strerror(errno);
        reader_guards[i].Reset(readers[i]);
        if (readers[i] == 0) {
            close(data[1]);
            close(ready[0]);
            const char marker = static_cast<char>('0' + i);
            if (!WriteAll(ready[1], &marker, 1)) {
                _exit(2);
            }
            close(ready[1]);
            char byte = 0;
            _exit(read(data[0], &byte, 1) == 1 ? 0 : 3);
        }
    }

    close(ready[1]);
    char markers[2] = {};
    ASSERT_TRUE(ReadExactly(ready[0], sizeof(markers))) << strerror(errno);
    close(ready[0]);

    for (pid_t reader : readers) {
        if (!WaitForSleepingProcess(reader)) {
            reader_guards[0].Cleanup();
            reader_guards[1].Cleanup();
            FAIL() << "reader did not enter the pipe wait path";
        }
    }

    const char tokens[2] = {'a', 'b'};
    ASSERT_TRUE(WriteAll(data[1], tokens, sizeof(tokens))) << strerror(errno);

    for (pid_t reader : readers) {
        int status = 0;
        if (!WaitForChild(reader, &status)) {
            FAIL() << "reader baton did not make finite progress";
        }
        const size_t index = reader == readers[0] ? 0 : 1;
        reader_guards[index].Release();
        ASSERT_TRUE(WIFEXITED(status));
        EXPECT_EQ(0, WEXITSTATUS(status));
    }

    close(data[0]);
    close(data[1]);
}

TEST(PipeWaitqueueWakeup, ResizeWakesAtomicWriterAfterThresholdCrossing) {
    int data[2] = {-1, -1};
    int ready[2] = {-1, -1};
    ASSERT_EQ(0, pipe(data)) << strerror(errno);
    ASSERT_EQ(0, pipe(ready)) << strerror(errno);
    ASSERT_EQ(4096, fcntl(data[1], F_SETPIPE_SZ, 4096)) << strerror(errno);
    const char occupied = 'x';
    ASSERT_EQ(1, write(data[1], &occupied, 1)) << strerror(errno);

    pid_t writer = fork();
    ASSERT_GE(writer, 0) << strerror(errno);
    ChildProcessGuard writer_guard(writer);
    if (writer == 0) {
        close(data[0]);
        close(ready[0]);
        const char marker = 'R';
        if (!WriteAll(ready[1], &marker, 1)) {
            _exit(2);
        }
        close(ready[1]);
        std::vector<char> payload(4096, 'r');
        _exit(WriteAll(data[1], payload.data(), payload.size()) ? 0 : 3);
    }

    close(ready[1]);
    char marker = 0;
    ASSERT_EQ(1, read(ready[0], &marker, 1)) << strerror(errno);
    ASSERT_EQ('R', marker);
    close(ready[0]);
    if (!WaitForSleepingProcess(writer)) {
        writer_guard.Cleanup();
        FAIL() << "atomic writer did not enter the pipe wait path";
    }

    ASSERT_EQ(8192, fcntl(data[1], F_SETPIPE_SZ, 8192)) << strerror(errno);
    int status = 0;
    if (!WaitForChild(writer, &status)) {
        FAIL() << "pipe resize did not wake the newly eligible writer";
    }
    writer_guard.Release();
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));

    close(data[0]);
    close(data[1]);
}

TEST(PipeWaitqueueWakeup, BlockingReadConsumesChildReadyByte) {
    int ready_pipe[2] = {-1, -1};
    int release_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(ready_pipe)) << strerror(errno);
    ASSERT_EQ(0, pipe(release_pipe)) << strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        close(ready_pipe[0]);
        close(release_pipe[1]);

        for (int i = 0; i < 200; ++i) {
            char ready = 'r';
            if (write(ready_pipe[1], &ready, 1) != 1) {
                _exit(2);
            }

            char release = 0;
            ssize_t n = read(release_pipe[0], &release, 1);
            if (n != 1 || release != 'c') {
                _exit(3);
            }
        }
        close(ready_pipe[1]);
        close(release_pipe[0]);
        _exit(0);
    }

    close(ready_pipe[1]);
    close(release_pipe[0]);

    for (int i = 0; i < 200; ++i) {
        char ready = 0;
        ASSERT_EQ(1, read(ready_pipe[0], &ready, 1)) << strerror(errno);
        ASSERT_EQ('r', ready);

        char release = 'c';
        ASSERT_EQ(1, write(release_pipe[1], &release, 1)) << strerror(errno);
    }
    close(ready_pipe[0]);
    close(release_pipe[1]);

    int status = 0;
    if (!WaitForChild(child, &status)) {
        kill(child, SIGKILL);
        waitpid(child, nullptr, 0);
        FAIL() << "child did not finish pipe wakeup handshake";
    }
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
