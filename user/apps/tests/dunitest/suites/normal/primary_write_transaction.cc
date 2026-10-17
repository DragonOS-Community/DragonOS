#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <algorithm>
#include <string>
#include <vector>

namespace {

constexpr size_t kChunk = 64 * 1024;

class TestFile {
 public:
    TestFile() {
        const char* directory = getenv("DUNITEST_PRIMARY_WRITE_DIR");
        path_ = std::string(directory ? directory : "/tmp") + "/primary-write-XXXXXX";
        fd_ = mkstemp(path_.data());
    }
    ~TestFile() {
        if (fd_ >= 0) {
            close(fd_);
            unlink(path_.c_str());
        }
    }
    int fd() const { return fd_; }

 private:
    std::string path_;
    int fd_ = -1;
};

long long monotonic_ns() {
    timespec ts{};
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) return -1;
    return static_cast<long long>(ts.tv_sec) * 1000000000 + ts.tv_nsec;
}

// The parent owns cleanup and does not share the tested MM. A same-file
// source fault must not hang the suite forever beneath a filesystem lock.
template <typename F>
void run_child(F operation) {
    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) _exit(operation());

    const long long start = monotonic_ns();
    int status = 0;
    for (;;) {
        const pid_t result = waitpid(child, &status, WNOHANG);
        if (result == child) {
            ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
            EXPECT_EQ(0, WEXITSTATUS(status));
            return;
        }
        if (result < 0 && errno != EINTR) {
            // ECHILD means there is no owned child left; do not signal a PID
            // that might already have been recycled.
            FAIL() << "waitpid: " << strerror(errno);
            return;
        }
        const long long now = monotonic_ns();
        if (start < 0 || now < 0 || now - start >= 15000000000LL) break;
        usleep(10000);
    }
    // The unreaped child still owns its PID even if it exits concurrently.
    kill(child, SIGKILL);
    while (waitpid(child, &status, 0) < 0 && errno == EINTR) {}
    FAIL() << "primary write timed out";
}

bool matches(int fd, off_t offset, const std::vector<unsigned char>& expected) {
    unsigned char buffer[4096];
    size_t completed = 0;
    while (completed < expected.size()) {
        const size_t length = std::min(sizeof(buffer), expected.size() - completed);
        if (pread(fd, buffer, length, offset + completed) != static_cast<ssize_t>(length))
            return false;
        if (memcmp(buffer, expected.data() + completed, length) != 0) return false;
        completed += length;
    }
    return true;
}

}  // namespace

TEST(PrimaryWriteTransaction, LargeSameFileMappedSourceCompletesOneWrite) {
    TestFile file;
    ASSERT_GE(file.fd(), 0) << strerror(errno);
    run_child([&] {
        const size_t length = 3 * kChunk + 31;
        std::vector<unsigned char> bytes(length);
        for (size_t i = 0; i < length; ++i) bytes[i] = (i * 17 + i / 4096) & 255;
        if (pwrite(file.fd(), bytes.data(), length, 0) != static_cast<ssize_t>(length)) return 1;
        void* source = mmap(nullptr, length, PROT_READ, MAP_PRIVATE, file.fd(), 0);
        if (source == MAP_FAILED) return 2;
        // Do not touch the source first: each chunk must be allowed to fault
        // before the destination's page-cache invalidation exclusion.
        if (madvise(source, length, MADV_DONTNEED) != 0) {
            munmap(source, length);
            return 3;
        }
        const ssize_t written = pwrite(file.fd(), source, length, length);
        const int unmapped = munmap(source, length);
        if (written != static_cast<ssize_t>(length) || unmapped != 0) return 4;
        struct stat metadata{};
        if (fstat(file.fd(), &metadata) != 0 || metadata.st_size != static_cast<off_t>(2 * length))
            return 5;
        if (lseek(file.fd(), 0, SEEK_CUR) != 0) return 6;
        return matches(file.fd(), 0, bytes) && matches(file.fd(), length, bytes) ? 0 : 7;
    });
}

TEST(PrimaryWriteTransaction, InaccessibleTailReturnsOnlyCommittedPrefix) {
    TestFile file;
    ASSERT_GE(file.fd(), 0) << strerror(errno);
    run_child([&] {
        void* source = mmap(nullptr, 2 * kChunk, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (source == MAP_FAILED) return 1;
        memset(source, 0x6b, kChunk);
        if (mprotect(static_cast<unsigned char*>(source) + kChunk, kChunk, PROT_NONE) != 0) {
            munmap(source, 2 * kChunk);
            return 2;
        }
        const ssize_t written = pwrite(file.fd(), source, 2 * kChunk, 0);
        munmap(source, 2 * kChunk);
        if (written != static_cast<ssize_t>(kChunk)) return 3;
        struct stat metadata{};
        if (fstat(file.fd(), &metadata) != 0 || metadata.st_size != static_cast<off_t>(kChunk))
            return 4;
        if (lseek(file.fd(), 0, SEEK_CUR) != 0) return 5;
        return matches(file.fd(), 0, std::vector<unsigned char>(kChunk, 0x6b)) ? 0 : 6;
    });
}

TEST(PrimaryWriteTransaction, AppendAcrossChunksAdvancesOffsetOnce) {
    TestFile file;
    ASSERT_GE(file.fd(), 0) << strerror(errno);
    run_child([&] {
        if (fcntl(file.fd(), F_SETFL, O_APPEND) != 0) return 1;
        const size_t length = 3 * kChunk + 31;
        const std::vector<unsigned char> first(length, 0x31);
        const std::vector<unsigned char> second(length, 0x72);
        if (write(file.fd(), first.data(), length) != static_cast<ssize_t>(length)) return 2;
        if (lseek(file.fd(), 0, SEEK_SET) != 0) return 3;
        if (write(file.fd(), second.data(), length) != static_cast<ssize_t>(length)) return 4;
        if (lseek(file.fd(), 0, SEEK_CUR) != static_cast<off_t>(2 * length)) return 5;
        return matches(file.fd(), 0, first) && matches(file.fd(), length, second) ? 0 : 6;
    });
}

namespace {
ssize_t vector_write(int operation, int fd, const iovec* vectors, int count, off_t offset) {
    if (operation == 0) return writev(fd, vectors, count);
    if (operation == 1) return pwritev(fd, vectors, count, offset);
    return syscall(SYS_pwritev2, fd, vectors, count, offset, 0UL, 0U);
}
}  // namespace

TEST(PrimaryWriteTransaction, VectorSourceCrossesChunksWithoutLosingIovBoundaries) {
    for (int operation : {0, 1, 2}) {
        SCOPED_TRACE(operation);
        TestFile file;
        ASSERT_GE(file.fd(), 0);
        run_child([&] {
            const size_t length = 3 * kChunk + 31;
            std::vector<unsigned char> bytes(length);
            for (size_t i = 0; i < length; ++i) bytes[i] = (i * 17 + i / 4096) & 255;
            iovec vectors[] = {{bytes.data(), 4093}, {bytes.data() + 4093, kChunk + 19},
                              {bytes.data() + 4093 + kChunk + 19, length - 4093 - kChunk - 19}};
            if (vector_write(operation, file.fd(), vectors, 3, 0) != static_cast<ssize_t>(length)) return 1;
            if (lseek(file.fd(), 0, SEEK_CUR) != (operation == 0 ? static_cast<off_t>(length) : 0)) return 2;
            return matches(file.fd(), 0, bytes) ? 0 : 3;
        });
    }
}

TEST(PrimaryWriteTransaction, VectorFaultAcrossChunksCommitsOnlyExactPrefix) {
    for (int operation : {0, 1, 2}) {
        SCOPED_TRACE(operation);
        TestFile file;
        ASSERT_GE(file.fd(), 0);
        run_child([&] {
            const size_t length = 3 * kChunk;
            void* source = mmap(nullptr, length, PROT_READ | PROT_WRITE,
                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if (source == MAP_FAILED) return 1;
            memset(source, 'Q', 2 * kChunk);
            if (mprotect(static_cast<char*>(source) + 2 * kChunk, kChunk, PROT_NONE) != 0) return 2;
            iovec vectors[] = {{source, kChunk - 17},
                              {static_cast<char*>(source) + kChunk - 17, kChunk + 17},
                              {static_cast<char*>(source) + 2 * kChunk, kChunk}};
            ssize_t result = vector_write(operation, file.fd(), vectors, 3, 0);
            munmap(source, length);
            if (result != static_cast<ssize_t>(2 * kChunk)) return 3;
            struct stat metadata{};
            if (fstat(file.fd(), &metadata) != 0 || metadata.st_size != static_cast<off_t>(2 * kChunk)) return 4;
            if (lseek(file.fd(), 0, SEEK_CUR) != (operation == 0 ? static_cast<off_t>(2 * kChunk) : 0)) return 5;
            return matches(file.fd(), 0, std::vector<unsigned char>(2 * kChunk, 'Q')) ? 0 : 6;
        });
    }
}

TEST(PrimaryWriteTransaction, VectorSameFileMappedSourceCanFaultInsideTransaction) {
    for (int operation : {0, 1, 2}) {
        SCOPED_TRACE(operation);
        TestFile file;
        ASSERT_GE(file.fd(), 0);
        run_child([&] {
            const size_t length = 3 * kChunk + 31;
            std::vector<unsigned char> bytes(length);
            for (size_t i = 0; i < length; ++i) bytes[i] = (i * 29 + i / 4096) & 255;
            if (pwrite(file.fd(), bytes.data(), length, 0) != static_cast<ssize_t>(length)) return 1;
            void* source = mmap(nullptr, length, PROT_READ, MAP_PRIVATE, file.fd(), 0);
            if (source == MAP_FAILED) return 2;
            if (madvise(source, length, MADV_DONTNEED) != 0) return 3;
            iovec vectors[] = {{source, kChunk - 17},
                              {static_cast<char*>(source) + kChunk - 17, kChunk + 35},
                              {static_cast<char*>(source) + 2 * kChunk + 18, length - 2 * kChunk - 18}};
            if (lseek(file.fd(), length, SEEK_SET) != static_cast<off_t>(length)) return 4;
            ssize_t result = vector_write(operation, file.fd(), vectors, 3, length);
            munmap(source, length);
            if (result != static_cast<ssize_t>(length)) return 5;
            if (lseek(file.fd(), 0, SEEK_CUR) != static_cast<off_t>((operation == 0 ? 2 : 1) * length)) return 6;
            return matches(file.fd(), 0, bytes) && matches(file.fd(), length, bytes) ? 0 : 7;
        });
    }
}

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
