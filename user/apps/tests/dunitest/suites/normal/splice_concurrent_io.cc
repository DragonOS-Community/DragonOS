#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/utsname.h>
#include <unistd.h>

#include <atomic>
#include <string>
#include <vector>

namespace {

constexpr char kByte = 0x01;
constexpr int kIterations = 1000;

void AlarmHandler(int) {
    _exit(124);
}

class TempFile {
  public:
    TempFile() {
        char tmpl[] = "/tmp/dunitest_splice_concurrent_XXXXXX";
        fd_ = mkstemp(tmpl);
        if (fd_ >= 0) {
            strcpy(path_, tmpl);
        }
    }

    ~TempFile() {
        if (fd_ >= 0) {
            close(fd_);
        }
        if (path_[0] != '\0') {
            unlink(path_);
        }
    }

    TempFile(const TempFile&) = delete;
    TempFile& operator=(const TempFile&) = delete;

    bool valid() const {
        return fd_ >= 0;
    }

    int fd() const {
        return fd_;
    }

  private:
    int fd_ = -1;
    char path_[sizeof("/tmp/dunitest_splice_concurrent_XXXXXX")] = {};
};

struct TestState {
    std::atomic<bool> done {false};
    std::atomic<int> failures {0};
    int file_fd = -1;
    int pipe_read_fd = -1;
    void* mapping = MAP_FAILED;
};

void* FileReaderThread(void* arg) {
    auto* state = static_cast<TestState*>(arg);
    while (!state->done.load(std::memory_order_acquire)) {
        char byte = 0;
        if (lseek(state->file_fd, 0, SEEK_SET) < 0) {
            state->failures.fetch_add(1, std::memory_order_relaxed);
            continue;
        }

        const ssize_t n = read(state->file_fd, &byte, 1);
        if (n < 0) {
            if (errno != EINTR) {
                state->failures.fetch_add(1, std::memory_order_relaxed);
            }
            continue;
        }
        if (n == 1 && byte != kByte) {
            state->failures.fetch_add(1, std::memory_order_relaxed);
        }
    }
    return nullptr;
}

void* PipeReaderThread(void* arg) {
    auto* state = static_cast<TestState*>(arg);
    while (!state->done.load(std::memory_order_acquire)) {
        char byte = 0;
        const ssize_t n = read(state->pipe_read_fd, &byte, 1);
        if (n < 0) {
            if (errno != EINTR) {
                state->failures.fetch_add(1, std::memory_order_relaxed);
            }
            continue;
        }
        if (n == 1 && byte != kByte) {
            state->failures.fetch_add(1, std::memory_order_relaxed);
        }
    }
    return nullptr;
}

void* MadviseThread(void* arg) {
    auto* state = static_cast<TestState*>(arg);
    while (!state->done.load(std::memory_order_acquire)) {
        madvise(state->mapping, 4096, MADV_DONTNEED);
    }
    return nullptr;
}

void FillPipeLeavingSpace(int fd, int capacity, size_t free_space) {
    ASSERT_GT(capacity, 0);
    ASSERT_LT(free_space, static_cast<size_t>(capacity));

    const size_t fill = static_cast<size_t>(capacity) - free_space;
    std::vector<char> bytes(fill, 0x7f);
    size_t written = 0;
    while (written < fill) {
        const ssize_t n = write(fd, bytes.data() + written, fill - written);
        ASSERT_GT(n, 0) << strerror(errno);
        written += static_cast<size_t>(n);
    }
}

bool IsDragonOS() {
    struct utsname uts {};
    if (uname(&uts) != 0) {
        return false;
    }
    return strstr(uts.release, "dragonos") != nullptr ||
           strstr(uts.nodename, "dragonos") != nullptr;
}

}  // namespace

TEST(SpliceConcurrentIo, FileToPipeNotStarvedByMadvise) {
    struct sigaction sa {};
    sa.sa_handler = AlarmHandler;
    ASSERT_EQ(0, sigaction(SIGALRM, &sa, nullptr)) << strerror(errno);
    alarm(20);

    TempFile file;
    ASSERT_TRUE(file.valid()) << strerror(errno);
    ASSERT_EQ(1, write(file.fd(), &kByte, 1)) << strerror(errno);
    ASSERT_EQ(1, write(file.fd(), &kByte, 1)) << strerror(errno);

    int pipe_fds[2] = {-1, -1};
    ASSERT_EQ(0, pipe(pipe_fds)) << strerror(errno);

    void* mapping = mmap(nullptr, 4096, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping) << strerror(errno);

    TestState state;
    state.file_fd = file.fd();
    state.pipe_read_fd = pipe_fds[0];
    state.mapping = mapping;

    pthread_t file_reader {};
    pthread_t pipe_reader {};
    pthread_t madvise_thread {};
    ASSERT_EQ(0, pthread_create(&file_reader, nullptr, FileReaderThread, &state))
        << strerror(errno);
    ASSERT_EQ(0, pthread_create(&pipe_reader, nullptr, PipeReaderThread, &state))
        << strerror(errno);
    ASSERT_EQ(0, pthread_create(&madvise_thread, nullptr, MadviseThread, &state))
        << strerror(errno);

    for (int i = 0; i < kIterations; ++i) {
        ASSERT_EQ(0, lseek(file.fd(), 0, SEEK_SET)) << strerror(errno);
        ASSERT_EQ(1, splice(file.fd(), nullptr, pipe_fds[1], nullptr, 1, 0))
            << strerror(errno);
    }

    state.done.store(true, std::memory_order_release);
    close(pipe_fds[1]);

    ASSERT_EQ(0, pthread_join(file_reader, nullptr)) << strerror(errno);
    ASSERT_EQ(0, pthread_join(pipe_reader, nullptr)) << strerror(errno);
    ASSERT_EQ(0, pthread_join(madvise_thread, nullptr)) << strerror(errno);

    close(pipe_fds[0]);
    munmap(mapping, 4096);
    alarm(0);

    EXPECT_EQ(0, state.failures.load(std::memory_order_relaxed));
}

TEST(SpliceConcurrentIo, FileToPipeShortReadNeedsOnlyAnyPipeSpace) {
    if (!IsDragonOS()) {
        GTEST_SKIP() << "DragonOS byte-ring pipe regression test";
    }

    struct sigaction sa {};
    sa.sa_handler = AlarmHandler;
    ASSERT_EQ(0, sigaction(SIGALRM, &sa, nullptr)) << strerror(errno);
    alarm(5);

    TempFile file;
    ASSERT_TRUE(file.valid()) << strerror(errno);
    ASSERT_EQ(1, write(file.fd(), &kByte, 1)) << strerror(errno);
    ASSERT_EQ(0, lseek(file.fd(), 0, SEEK_SET)) << strerror(errno);

    int pipe_fds[2] = {-1, -1};
    ASSERT_EQ(0, pipe(pipe_fds)) << strerror(errno);
    const int capacity = fcntl(pipe_fds[1], F_GETPIPE_SZ);
    ASSERT_GT(capacity, 1) << strerror(errno);
    FillPipeLeavingSpace(pipe_fds[1], capacity, 1);

    ASSERT_EQ(1, splice(file.fd(), nullptr, pipe_fds[1], nullptr, 4096, 0))
        << strerror(errno);

    close(pipe_fds[0]);
    close(pipe_fds[1]);
    alarm(0);
}

TEST(SpliceConcurrentIo, NonblockFileToPipeShortReadUsesAvailableSpace) {
    if (!IsDragonOS()) {
        GTEST_SKIP() << "DragonOS byte-ring pipe regression test";
    }

    TempFile file;
    ASSERT_TRUE(file.valid()) << strerror(errno);
    ASSERT_EQ(1, write(file.fd(), &kByte, 1)) << strerror(errno);
    ASSERT_EQ(0, lseek(file.fd(), 0, SEEK_SET)) << strerror(errno);

    int pipe_fds[2] = {-1, -1};
    ASSERT_EQ(0, pipe(pipe_fds)) << strerror(errno);
    const int capacity = fcntl(pipe_fds[1], F_GETPIPE_SZ);
    ASSERT_GT(capacity, 1) << strerror(errno);
    FillPipeLeavingSpace(pipe_fds[1], capacity, 1);

    ASSERT_EQ(1,
              splice(file.fd(), nullptr, pipe_fds[1], nullptr, 4096,
                     SPLICE_F_NONBLOCK))
        << strerror(errno);

    close(pipe_fds[0]);
    close(pipe_fds[1]);
}

TEST(SpliceConcurrentIo, NonblockFileToPipePipeBufNeedsCompleteSpace) {
    if (!IsDragonOS()) {
        GTEST_SKIP() << "DragonOS byte-ring pipe regression test";
    }

    TempFile file;
    ASSERT_TRUE(file.valid()) << strerror(errno);
    std::vector<char> bytes(4096, kByte);
    ASSERT_EQ(static_cast<ssize_t>(bytes.size()),
              write(file.fd(), bytes.data(), bytes.size()))
        << strerror(errno);
    ASSERT_EQ(0, lseek(file.fd(), 0, SEEK_SET)) << strerror(errno);

    int pipe_fds[2] = {-1, -1};
    ASSERT_EQ(0, pipe(pipe_fds)) << strerror(errno);
    const int capacity = fcntl(pipe_fds[1], F_GETPIPE_SZ);
    ASSERT_GT(capacity, 1) << strerror(errno);
    FillPipeLeavingSpace(pipe_fds[1], capacity, 1);

    errno = 0;
    EXPECT_EQ(-1,
              splice(file.fd(), nullptr, pipe_fds[1], nullptr, bytes.size(),
                     SPLICE_F_NONBLOCK));
    EXPECT_EQ(EAGAIN, errno);

    close(pipe_fds[0]);
    close(pipe_fds[1]);
}

TEST(SpliceConcurrentIo, ProcfsZeroSizeRegularFileCanSpliceData) {
    const int file_fd = open("/proc/cpuinfo", O_RDONLY);
    ASSERT_GE(file_fd, 0) << strerror(errno);

    struct stat st {};
    ASSERT_EQ(0, fstat(file_fd, &st)) << strerror(errno);
    ASSERT_TRUE(S_ISREG(st.st_mode));
    ASSERT_EQ(0, st.st_size);

    int pipe_fds[2] = {-1, -1};
    ASSERT_EQ(0, pipe(pipe_fds)) << strerror(errno);

    const ssize_t spliced = splice(file_fd, nullptr, pipe_fds[1], nullptr, 4096, 0);
    ASSERT_GT(spliced, 0) << strerror(errno);

    std::vector<char> bytes(static_cast<size_t>(spliced));
    ASSERT_EQ(spliced, read(pipe_fds[0], bytes.data(), bytes.size())) << strerror(errno);
    const std::string text(bytes.begin(), bytes.end());
    EXPECT_NE(std::string::npos, text.find("processor"));

    close(pipe_fds[0]);
    close(pipe_fds[1]);
    close(file_fd);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
