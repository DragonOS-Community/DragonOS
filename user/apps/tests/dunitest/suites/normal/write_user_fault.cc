#include <gtest/gtest.h>
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#include <string>
#include <vector>

namespace {
class Fd {
public:
    explicit Fd(int value = -1) : value_(value) {}
    ~Fd() { if (value_ >= 0) close(value_); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return value_; }
private:
    int value_;
};

class WriteUserFaultTest : public ::testing::Test {
protected:
    void SetUp() override {
        page_ = sysconf(_SC_PAGESIZE); ASSERT_GT(page_, 0);
        // Reserve both neighbors so subsequent allocations cannot refill the
        // unmapped middle page. All pointers used below are in valid user range.
        reservation_ = mmap(nullptr, page_ * 3, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        ASSERT_NE(MAP_FAILED, reservation_);
        prefix_ = static_cast<char*>(reservation_);
        memset(prefix_, 'p', page_);
        hole_ = prefix_ + page_;
        ASSERT_EQ(0, munmap(hole_, page_));
        char name[] = "/tmp/write_user_fault_XXXXXX";
        char* root = mkdtemp(name); ASSERT_NE(nullptr, root);
        root_ = root;
        ASSERT_EQ(0, mount("writefault", root_.c_str(), "tmpfs", 0, "mode=0777"));
        mounted_ = true;
        path_ = root_ + "/file";
    }
    void TearDown() override {
        if (mounted_) { EXPECT_EQ(0, umount(root_.c_str())); }
        if (!root_.empty()) { EXPECT_EQ(0, rmdir(root_.c_str())); }
        if (reservation_ != MAP_FAILED) {
            EXPECT_EQ(0, munmap(reservation_, page_));
            EXPECT_EQ(0, munmap(prefix_ + page_ * 2, page_));
        }
    }
    void check_prefix(int fd, size_t size) {
        std::vector<char> bytes(size);
        ASSERT_EQ(static_cast<ssize_t>(size), read(fd, bytes.data(), bytes.size()));
        for (char byte : bytes) ASSERT_EQ('p', byte);
    }
    long page_ = 0;
    void* reservation_ = MAP_FAILED;
    char* prefix_ = nullptr;
    char* hole_ = nullptr;
    std::string root_, path_;
    bool mounted_ = false;
};

TEST_F(WriteUserFaultTest, DescriptorAndAccessErrorsPrecedeUnmappedSource) {
    ASSERT_EQ(-1, syscall(SYS_write, -1, hole_, 1));
    EXPECT_EQ(EBADF, errno);
    { Fd created(open(path_.c_str(), O_CREAT | O_WRONLY, 0600)); ASSERT_GE(created.get(), 0); }
    Fd readonly(open(path_.c_str(), O_RDONLY)); ASSERT_GE(readonly.get(), 0);
    ASSERT_EQ(-1, syscall(SYS_write, readonly.get(), hole_, 1));
    EXPECT_EQ(EBADF, errno);
    struct stat st {}; ASSERT_EQ(0, fstat(readonly.get(), &st)); EXPECT_EQ(0, st.st_size);
}

TEST_F(WriteUserFaultTest, InvalidKernelRangeFailsBeforePrivilegeRemoval) {
    Fd fd(open(path_.c_str(), O_CREAT | O_RDWR, 0600)); ASSERT_GE(fd.get(), 0);
    ASSERT_EQ(0, fchown(fd.get(), 1000, 1000));
    ASSERT_EQ(0, fchmod(fd.get(), 06755));
    const pid_t child = fork(); ASSERT_GE(child, 0);
    if (child == 0) {
        if (setgroups(0, nullptr) != 0 || setgid(1000) != 0 || setuid(1000) != 0) _exit(2);
        const void* kernel = reinterpret_cast<const void*>(UINTPTR_MAX - 4095);
        const long rc = syscall(SYS_write, fd.get(), kernel, 1);
        const int error = errno;
        struct stat st {};
        _exit(rc == -1 && error == EFAULT && fstat(fd.get(), &st) == 0 &&
              (st.st_mode & (S_ISUID | S_ISGID)) == (S_ISUID | S_ISGID) && st.st_size == 0 ? 0 : 3);
    }
    int status = 0; ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status)); EXPECT_EQ(0, WEXITSTATUS(status));
    struct stat st {}; ASSERT_EQ(0, fstat(fd.get(), &st));
    EXPECT_EQ(static_cast<mode_t>(S_ISUID | S_ISGID), st.st_mode & (S_ISUID | S_ISGID));
}

TEST_F(WriteUserFaultTest, NativeFileRetainsBothPageAndPartialPagePrefixes) {
    Fd fd(open(path_.c_str(), O_CREAT | O_RDWR, 0600)); ASSERT_GE(fd.get(), 0);
    for (size_t prefix : {static_cast<size_t>(page_), size_t(16)}) {
        ASSERT_EQ(0, ftruncate(fd.get(), 0));
        ASSERT_EQ(0, lseek(fd.get(), 0, SEEK_SET));
        ASSERT_EQ(static_cast<ssize_t>(prefix), syscall(SYS_write, fd.get(), hole_ - prefix, prefix + 16));
        struct stat st {}; ASSERT_EQ(0, fstat(fd.get(), &st));
        EXPECT_EQ(static_cast<off_t>(prefix), st.st_size);
        EXPECT_EQ(static_cast<off_t>(prefix), lseek(fd.get(), 0, SEEK_CUR));
        ASSERT_EQ(0, lseek(fd.get(), 0, SEEK_SET));
        ASSERT_NO_FATAL_FAILURE(check_prefix(fd.get(), prefix));
    }
}

TEST_F(WriteUserFaultTest, PipeRetainsOneCompletePageBeforeFault) {
    int pair[2]; ASSERT_EQ(0, pipe(pair)); Fd reader(pair[0]), writer(pair[1]);
    ASSERT_EQ(page_, syscall(SYS_write, writer.get(), prefix_, page_ * 2));
    ASSERT_NO_FATAL_FAILURE(check_prefix(reader.get(), page_));
    ASSERT_EQ(0, fcntl(reader.get(), F_SETFL, O_NONBLOCK));
    char byte;
    EXPECT_EQ(-1, read(reader.get(), &byte, 1)); EXPECT_EQ(EAGAIN, errno);
}

TEST_F(WriteUserFaultTest, UnixStreamDoesNotSendPartialFirstMessageBlock) {
    int pair[2]; ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, pair));
    Fd reader(pair[0]), writer(pair[1]);
    ASSERT_EQ(-1, syscall(SYS_write, writer.get(), hole_ - 16, 32));
    EXPECT_EQ(EFAULT, errno);
    char byte;
    EXPECT_EQ(-1, recv(reader.get(), &byte, 1, MSG_DONTWAIT)); EXPECT_EQ(EAGAIN, errno);
}

TEST_F(WriteUserFaultTest, UnixStreamKeepsCompletedMessageBlocksBeforeFault) {
    int pair[2]; ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, pair));
    Fd reader(pair[0]), writer(pair[1]);
    int buffer = page_;
    ASSERT_EQ(0, setsockopt(writer.get(), SOL_SOCKET, SO_SNDBUF, &buffer, sizeof(buffer)));
    socklen_t size = sizeof(buffer);
    ASSERT_EQ(0, getsockopt(writer.get(), SOL_SOCKET, SO_SNDBUF, &buffer, &size));
    // Linux unix_stream_sendmsg limits each skb to half the send buffer minus
    // 64 bytes. This small buffer keeps that limit below its fragment cap.
    const ssize_t block = (buffer >> 1) - 64;
    ASSERT_GT(block, 0); ASSERT_LT(block, page_);
    ASSERT_EQ(block, syscall(SYS_write, writer.get(), prefix_, page_ + 16));
    ASSERT_NO_FATAL_FAILURE(check_prefix(reader.get(), block));
    char byte;
    EXPECT_EQ(-1, recv(reader.get(), &byte, 1, MSG_DONTWAIT)); EXPECT_EQ(EAGAIN, errno);
}

TEST_F(WriteUserFaultTest, UnixDatagramFaultDoesNotSendPartialPacket) {
    int pair[2]; ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_DGRAM, 0, pair));
    Fd reader(pair[0]), writer(pair[1]);
    ASSERT_EQ(-1, syscall(SYS_write, writer.get(), hole_ - 16, 32));
    EXPECT_EQ(EFAULT, errno);
    char bytes[32];
    EXPECT_EQ(-1, recv(reader.get(), bytes, sizeof(bytes), MSG_DONTWAIT)); EXPECT_EQ(EAGAIN, errno);
    ASSERT_EQ(4, write(writer.get(), "good", 4));
    ASSERT_EQ(4, recv(reader.get(), bytes, sizeof(bytes), MSG_DONTWAIT));
    EXPECT_EQ(0, memcmp(bytes, "good", 4));
}
}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
