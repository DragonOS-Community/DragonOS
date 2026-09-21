#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <array>
#include <atomic>

namespace {
constexpr int kReaders = 32;
constexpr int kReads = 64;
constexpr const char* kDevice = "/dev/vda";

struct ReadStress {
    std::array<unsigned char, 512> expected;
    std::atomic<int> ready{0};
    std::atomic<int> failures{0};
    std::atomic<bool> start{false};
};

void* ReadWorker(void* arg) {
    auto* state = static_cast<ReadStress*>(arg);
    // Separate open descriptions avoid VFS File::private_data serialization.
    const int fd = open(kDevice, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        ++state->failures;
    }
    ++state->ready;
    while (!state->start.load()) {
        usleep(1000);
    }
    if (fd >= 0) {
        std::array<unsigned char, 512> data;
        for (int i = 0; i < kReads; ++i) {
            ssize_t count;
            do {
                count = pread(fd, data.data(), data.size(), 0);
            } while (count < 0 && errno == EINTR);
            if (count != static_cast<ssize_t>(data.size()) || data != state->expected) {
                ++state->failures;
                break;
            }
        }
        if (close(fd) != 0) {
            ++state->failures;
        }
    }
    return nullptr;
}

void RunChild() {
    ReadStress state;
    const int fd = open(kDevice, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        _exit(2);
    }
    const ssize_t count = pread(fd, state.expected.data(), state.expected.size(), 0);
    const int closed = close(fd);
    if (count != static_cast<ssize_t>(state.expected.size()) || closed != 0) {
        _exit(2);
    }
    pthread_t threads[kReaders];
    int created = 0;
    for (; created < kReaders; ++created) {
        if (pthread_create(&threads[created], nullptr, ReadWorker, &state) != 0) {
            ++state.failures;
            break;
        }
    }
    while (state.ready.load() < created) {
        usleep(1000);
    }
    state.start = true;
    for (int i = 0; i < created; ++i) {
        if (pthread_join(threads[i], nullptr) != 0) {
            ++state.failures;
        }
    }
    _exit(state.failures.load() == 0 ? 0 : 1);
}
}  // namespace

TEST(VirtioBlkBackpressure, ConcurrentIndependentReadsCompleteWithoutIoErrors) {
    int fd = open(kDevice, O_RDONLY | O_CLOEXEC);
    if (fd < 0 && (errno == ENOENT || errno == ENODEV || errno == EACCES)) {
        GTEST_SKIP() << "Read-only VirtIO device unavailable: " << strerror(errno);
    }
    ASSERT_GE(fd, 0) << strerror(errno);
    struct stat st = {};
    const int stat_result = fstat(fd, &st);
    if (stat_result != 0 || !S_ISBLK(st.st_mode)) {
        close(fd);
        FAIL() << "Expected a block device";
    }
    // LBA 0 contains stable partition/boot data, not changing filesystem metadata.
    EXPECT_EQ(0, close(fd));
    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        RunChild();
    }
    // Keep the test controller bounded even when a lost BIO blocks a reader.
    // SIGKILL cannot guarantee cleanup of an uninterruptible kernel waiter;
    // the external guest runner must terminate the VM after such a failure.
    timespec start = {};
    ASSERT_EQ(0, clock_gettime(CLOCK_MONOTONIC, &start));
    int status = 0;
    for (;;) {
        const pid_t result = waitpid(child, &status, WNOHANG);
        if (result == child) {
            ASSERT_TRUE(WIFEXITED(status)) << status;
            EXPECT_EQ(0, WEXITSTATUS(status));
            return;
        }
        ASSERT_TRUE(result == 0 || (result < 0 && errno == EINTR)) << strerror(errno);
        timespec now = {};
        ASSERT_EQ(0, clock_gettime(CLOCK_MONOTONIC, &now));
        if (now.tv_sec - start.tv_sec >= 60) {
            kill(child, SIGKILL);
            // Never block on reaping a task stuck in uninterruptible I/O.
            waitpid(child, &status, WNOHANG);
            FAIL() << "Concurrent raw reads timed out; stop this guest before further tests";
        }
        usleep(10000);
    }
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
