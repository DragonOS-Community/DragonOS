#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <array>
#include <vector>

namespace {
class Descriptor {
 public:
    explicit Descriptor(int fd) : fd_(fd) {}
    ~Descriptor() { if (fd_ >= 0) close(fd_); }
    Descriptor(const Descriptor&) = delete;
    Descriptor& operator=(const Descriptor&) = delete;
    int get() const { return fd_; }
 private:
    int fd_;
};
class Mapping {
 public:
    Mapping(size_t length, int flags) : size_(length) {
        data = static_cast<unsigned char*>(mmap(nullptr, length, PROT_READ | PROT_WRITE,
                                                flags | MAP_ANONYMOUS, -1, 0));
    }
    ~Mapping() { if (data != MAP_FAILED) munmap(data, size_); }
    Mapping(const Mapping&) = delete;
    Mapping& operator=(const Mapping&) = delete;
    unsigned char* data;
 private:
    size_t size_;
};
// Keep a child bounded even when an assertion returns before the normal wait.
class Child {
 public:
    explicit Child(pid_t pid) : pid_(pid) {}
    ~Child() {
        if (pid_ > 0) {
            kill(pid_, SIGKILL);
            int status;
            while (waitpid(pid_, &status, 0) < 0 && errno == EINTR) {}
        }
    }
    bool Wait(int* status) {
        timespec start{};
        if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) return false;
        for (;;) {
            const pid_t result = waitpid(pid_, status, WNOHANG);
            if (result == pid_) { pid_ = -1; return true; }
            if (result < 0 && errno != EINTR) {
                if (errno == ECHILD) pid_ = -1;
                return false;
            }
            timespec now{};
            if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return false;
            if ((now.tv_sec - start.tv_sec) * 1000000000LL + now.tv_nsec - start.tv_nsec
                >= 3000000000LL) return false;
            sched_yield();
        }
    }
 private:
    pid_t pid_;
};
bool WaitForReceiver(pid_t pid) {
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/stat", pid);
    timespec start{};
    if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) return false;
    for (;;) {
        Descriptor stat(open(path, O_RDONLY | O_CLOEXEC));
        if (stat.get() < 0) return false;
        char data[512];
        ssize_t count = read(stat.get(), data, sizeof(data) - 1);
        if (count <= 0) return false;
        data[count] = 0;
        const char* end = strrchr(data, ')');
        if (end && end[1] == ' ' && end[2] == 'S') return true;
        if (end && end[1] == ' ' && end[2] == 'Z') return false;
        timespec now{};
        if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return false;
        if ((now.tv_sec - start.tv_sec) * 1000000000LL + now.tv_nsec - start.tv_nsec
            >= 3000000000LL) return false;
        sched_yield();
    }
}
int TemporaryFile() {
    char path[] = "/tmp/iov-snapshot-XXXXXX";
    const int fd = mkstemp(path);
    if (fd >= 0) unlink(path);
    return fd;
}
TEST(IovSnapshot, WritesAcrossMetadataStorageBoundaries) {
    Descriptor fd(TemporaryFile());
    ASSERT_GE(fd.get(), 0);
    std::array<unsigned char, 1024> source{}, output{};
    std::array<iovec, 1024> vectors{};
    for (size_t i = 0; i < source.size(); ++i) {
        source[i] = (i * 37 + 11) & 255;
        vectors[i] = {source.data() + i, 1};
    }
    for (size_t count : {size_t(256), size_t(257), size_t(1024)}) {
        SCOPED_TRACE(count);
        ASSERT_EQ(0, ftruncate(fd.get(), 0));
        ASSERT_EQ(0, lseek(fd.get(), 0, SEEK_SET));
        ASSERT_EQ(static_cast<ssize_t>(count), writev(fd.get(), vectors.data(), count));
        EXPECT_EQ(static_cast<off_t>(count), lseek(fd.get(), 0, SEEK_CUR));
        ASSERT_EQ(static_cast<ssize_t>(count), pread(fd.get(), output.data(), count, 0));
        EXPECT_EQ(0, memcmp(source.data(), output.data(), count));
    }
}
TEST(IovSnapshot, ReadsAcrossMetadataStorageBoundariesWithZeroEntries) {
    Descriptor fd(TemporaryFile());
    ASSERT_GE(fd.get(), 0);
    std::array<unsigned char, 1024> source{}, output{};
    for (size_t i = 0; i < source.size(); ++i) source[i] = (i * 37 + 11) & 255;
    ASSERT_EQ(1024, write(fd.get(), source.data(), source.size()));
    for (size_t count : {size_t(256), size_t(257), size_t(1024)}) {
        SCOPED_TRACE(count);
        std::array<iovec, 1024> vectors{};
        size_t bytes = 0;
        for (size_t i = 0; i < count; ++i) {
            const bool empty = i % 3 == 0;
            vectors[i] = {empty ? nullptr : output.data() + bytes, empty ? 0U : 1U};
            if (!empty) ++bytes;
        }
        ASSERT_EQ(0, lseek(fd.get(), 0, SEEK_SET));
        ASSERT_EQ(static_cast<ssize_t>(bytes), readv(fd.get(), vectors.data(), count));
        EXPECT_EQ(static_cast<off_t>(bytes), lseek(fd.get(), 0, SEEK_CUR));
        EXPECT_EQ(0, memcmp(source.data(), output.data(), bytes));
    }
}
TEST(IovSnapshot, LaterMetadataFaultDoesNotCommitTheImportedPrefix) {
    const size_t page = sysconf(_SC_PAGESIZE);
    Mapping metadata(page * 2, MAP_PRIVATE);
    ASSERT_NE(MAP_FAILED, metadata.data);
    auto* vectors = reinterpret_cast<iovec*>(metadata.data);
    const size_t boundary = page / sizeof(iovec);
    char value = 'x';
    for (size_t i = 0; i < boundary; ++i) vectors[i] = {&value, 1};
    ASSERT_EQ(0, mprotect(metadata.data + page, page, PROT_NONE));
    Descriptor fd(TemporaryFile());
    ASSERT_GE(fd.get(), 0);
    errno = 0;
    EXPECT_EQ(-1, writev(fd.get(), vectors, boundary + 1));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_EQ(0, lseek(fd.get(), 0, SEEK_CUR));
    struct stat info{};
    ASSERT_EQ(0, fstat(fd.get(), &info));
    EXPECT_EQ(0, info.st_size);
}
TEST(IovSnapshot, SignedLengthPrecedesTheNextMetadataPageFault) {
    const size_t page = sysconf(_SC_PAGESIZE);
    Mapping metadata(page * 2, MAP_PRIVATE);
    ASSERT_NE(MAP_FAILED, metadata.data);
    auto* vectors = reinterpret_cast<iovec*>(metadata.data);
    const size_t boundary = page / sizeof(iovec);
    char value = 'x';
    for (size_t i = 0; i < boundary; ++i) vectors[i] = {&value, 1};
    ASSERT_EQ(0, mprotect(metadata.data + page, page, PROT_NONE));
    Descriptor fd(TemporaryFile());
    ASSERT_GE(fd.get(), 0);
    for (size_t index : {size_t(0), boundary - 1}) {
        SCOPED_TRACE(index);
        vectors[index].iov_len = SIZE_MAX;
        errno = 0;
        EXPECT_EQ(-1, writev(fd.get(), vectors, boundary + 1));
        EXPECT_EQ(EINVAL, errno);
        EXPECT_EQ(0, lseek(fd.get(), 0, SEEK_CUR));
        vectors[index].iov_len = 1;
    }
}
TEST(IovSnapshot, BlockedRecvmsgRetainsItsImportedDestination) {
    const size_t page = sysconf(_SC_PAGESIZE);
    Mapping shared(page * 3, MAP_SHARED);
    ASSERT_NE(MAP_FAILED, shared.data);
    auto* vectors = reinterpret_cast<iovec*>(shared.data);
    const size_t count = page / sizeof(iovec) + 1;
    ASSERT_LT(count * sizeof(iovec), page * 2);
    unsigned char* original = shared.data + page * 2;
    unsigned char* replacement = original + 16;
    memset(original, 0xcd, 8);
    memset(replacement, 0xcd, 8);
    vectors[count - 1] = {original, 8};
    int pair[2];
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0, pair));
    Descriptor sender(pair[0]), receiver(pair[1]);
    int ready[2];
    ASSERT_EQ(0, pipe2(ready, O_CLOEXEC));
    Descriptor ready_read(ready[0]), ready_write(ready[1]);
    const pid_t pid = fork();
    ASSERT_GE(pid, 0);
    if (pid == 0) {
        alarm(10);
        msghdr message{};
        message.msg_iov = vectors;
        message.msg_iovlen = count;
        const char marker = 'r';
        if (write(ready_write.get(), &marker, 1) != 1) _exit(10);
        _exit(recvmsg(receiver.get(), &message, 0) == 8 ? 0 : 11);
    }
    Child cleanup(pid);
    pollfd readable{ready_read.get(), POLLIN, 0};
    ASSERT_EQ(1, poll(&readable, 1, 2000));
    char marker = 0;
    ASSERT_EQ(1, read(ready_read.get(), &marker, 1));
    ASSERT_EQ('r', marker);
    // After this handshake the child only enters recvmsg. Its sleeping state
    // establishes that import and receive admission preceded our metadata edit.
    ASSERT_TRUE(WaitForReceiver(pid));
    vectors[count - 1].iov_base = replacement;
    std::vector<iovec> send_vectors(count);
    send_vectors.back() = {const_cast<char*>("snapshot"), 8};
    msghdr message{};
    message.msg_iov = send_vectors.data();
    message.msg_iovlen = send_vectors.size();
    ASSERT_EQ(8, sendmsg(sender.get(), &message, MSG_DONTWAIT));
    int status = 0;
    ASSERT_TRUE(cleanup.Wait(&status));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_EQ(0, memcmp(original, "snapshot", 8));
    const std::array<unsigned char, 8> canary = {0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd, 0xcd};
    EXPECT_EQ(0, memcmp(replacement, canary.data(), canary.size()));
}
}  // namespace
int main(int argc, char** argv) { testing::InitGoogleTest(&argc, argv); return RUN_ALL_TESTS(); }
