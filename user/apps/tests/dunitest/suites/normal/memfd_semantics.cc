#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <gtest/gtest.h>

#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <string>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef MFD_CLOEXEC
#define MFD_CLOEXEC 0x0001
#define MFD_ALLOW_SEALING 0x0002
#define MFD_HUGETLB 0x0004
#define MFD_NOEXEC_SEAL 0x0008
#define MFD_EXEC 0x0010
#endif
#ifndef MFD_NOEXEC_SEAL
#define MFD_NOEXEC_SEAL 0x0008
#define MFD_EXEC 0x0010
#endif
#ifndef F_SEAL_FUTURE_WRITE
#define F_SEAL_FUTURE_WRITE 0x0010
#endif
#ifndef F_SEAL_EXEC
#define F_SEAL_EXEC 0x0020
#endif

namespace {
constexpr size_t kPage = 4096;

int Memfd(const char* name, unsigned flags) {
    return static_cast<int>(syscall(SYS_memfd_create, name, flags));
}

class Fd {
public:
    explicit Fd(int fd) : fd_(fd) {}
    ~Fd() { if (fd_ >= 0) close(fd_); }
    int get() const { return fd_; }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
private:
    int fd_;
};

TEST(Memfd, CreationAndRawName) {
    Fd fd(Memfd("example", MFD_CLOEXEC | MFD_ALLOW_SEALING));
    ASSERT_GE(fd.get(), 0) << strerror(errno);
    EXPECT_EQ(FD_CLOEXEC, fcntl(fd.get(), F_GETFD) & FD_CLOEXEC);
    EXPECT_EQ(0, fcntl(fd.get(), F_GET_SEALS));
    struct stat st {};
    ASSERT_EQ(0, fstat(fd.get(), &st));
    EXPECT_TRUE(S_ISREG(st.st_mode));
    EXPECT_EQ(0, static_cast<int>(st.st_size));
    EXPECT_EQ(0u, static_cast<unsigned>(st.st_nlink));

    std::string link = "/proc/self/fd/" + std::to_string(fd.get());
    char target[128] = {};
    ssize_t n = readlink(link.c_str(), target, sizeof(target));
    ASSERT_GT(n, 0);
    EXPECT_EQ("/memfd:example (deleted)", std::string(target, n));

    const char raw[] = {'x', static_cast<char>(0xff), 'y', 0};
    Fd raw_fd(Memfd(raw, MFD_ALLOW_SEALING));
    ASSERT_GE(raw_fd.get(), 0) << strerror(errno);
    link = "/proc/self/fd/" + std::to_string(raw_fd.get());
    n = readlink(link.c_str(), target, sizeof(target));
    ASSERT_GT(n, 0);
    EXPECT_EQ(std::string("/memfd:") + std::string(raw, 3) + " (deleted)",
              std::string(target, n));
}

TEST(Memfd, FlagsAndErrors) {
    Fd default_fd(Memfd("", 0));
    ASSERT_GE(default_fd.get(), 0);
    EXPECT_EQ(F_SEAL_SEAL, fcntl(default_fd.get(), F_GET_SEALS));
    struct stat default_st {};
    ASSERT_EQ(0, fstat(default_fd.get(), &default_st));
    EXPECT_EQ(0777u, static_cast<unsigned>(default_st.st_mode & 0777));

    Fd noexec(Memfd("noexec", MFD_NOEXEC_SEAL));
    ASSERT_GE(noexec.get(), 0);
    EXPECT_EQ(F_SEAL_EXEC, fcntl(noexec.get(), F_GET_SEALS));
    struct stat st {};
    ASSERT_EQ(0, fstat(noexec.get(), &st));
    EXPECT_EQ(0666u, static_cast<unsigned>(st.st_mode & 0777));

    errno = 0;
    EXPECT_EQ(-1, Memfd("invalid", MFD_EXEC | MFD_NOEXEC_SEAL));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, Memfd("invalid", 0x20));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, Memfd(reinterpret_cast<const char*>(1), 0));
    EXPECT_EQ(EFAULT, errno);
    std::string too_long(250, 'x');
    errno = 0;
    EXPECT_EQ(-1, Memfd(too_long.c_str(), 0));
    EXPECT_EQ(EINVAL, errno);
}

TEST(Memfd, SealsArePerInodeAndEnforced) {
    Fd fd(Memfd("seals", MFD_ALLOW_SEALING));
    ASSERT_GE(fd.get(), 0);
    Fd duplicate(dup(fd.get()));
    ASSERT_GE(duplicate.get(), 0);
    ASSERT_EQ(0, ftruncate(fd.get(), kPage));
    ASSERT_EQ(1, pwrite(fd.get(), "x", 1, 0));
    ASSERT_EQ(0, fcntl(fd.get(), F_ADD_SEALS, F_SEAL_GROW | F_SEAL_SHRINK));
    EXPECT_EQ(F_SEAL_GROW | F_SEAL_SHRINK, fcntl(duplicate.get(), F_GET_SEALS));
    errno = 0;
    EXPECT_EQ(-1, ftruncate(fd.get(), kPage + 1));
    EXPECT_EQ(EPERM, errno);
    errno = 0;
    EXPECT_EQ(-1, ftruncate(fd.get(), kPage - 1));
    EXPECT_EQ(EPERM, errno);
    ASSERT_EQ(0, fcntl(duplicate.get(), F_ADD_SEALS, F_SEAL_WRITE));
    errno = 0;
    EXPECT_EQ(-1, pwrite(fd.get(), "y", 1, 0));
    EXPECT_EQ(EPERM, errno);
    ASSERT_EQ(0, fcntl(fd.get(), F_ADD_SEALS, F_SEAL_SEAL));
    errno = 0;
    EXPECT_EQ(-1, fcntl(fd.get(), F_ADD_SEALS, 0));
    EXPECT_EQ(EPERM, errno);
}

TEST(Memfd, GrowSealAllowsFilePrefixOfWrite) {
    Fd fd(Memfd("prefix", MFD_ALLOW_SEALING));
    ASSERT_GE(fd.get(), 0);
    ASSERT_EQ(0, ftruncate(fd.get(), kPage));
    ASSERT_EQ(0, fcntl(fd.get(), F_ADD_SEALS, F_SEAL_GROW));
    std::string data(2 * kPage, 'P');
    EXPECT_EQ(static_cast<ssize_t>(kPage), pwrite(fd.get(), data.data(), data.size(), 0));
    EXPECT_EQ(1, pwrite(fd.get(), "QZ", 2, kPage - 1));
    errno = 0;
    EXPECT_EQ(-1, pwrite(fd.get(), "R", 1, kPage));
    EXPECT_EQ(EPERM, errno);
    struct stat st {};
    ASSERT_EQ(0, fstat(fd.get(), &st));
    EXPECT_EQ(static_cast<off_t>(kPage), st.st_size);
}

TEST(Memfd, SharedMappingAndFutureWrite) {
    Fd fd(Memfd("map", MFD_ALLOW_SEALING));
    ASSERT_GE(fd.get(), 0);
    ASSERT_EQ(0, ftruncate(fd.get(), kPage));
    void* old = mmap(nullptr, kPage, PROT_READ | PROT_WRITE, MAP_SHARED, fd.get(), 0);
    ASSERT_NE(MAP_FAILED, old) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, fcntl(fd.get(), F_ADD_SEALS, F_SEAL_WRITE));
    EXPECT_EQ(EBUSY, errno);
    ASSERT_EQ(0, fcntl(fd.get(), F_ADD_SEALS, F_SEAL_FUTURE_WRITE));
    static_cast<char*>(old)[0] = 'z';
    char c = 0;
    ASSERT_EQ(1, pread(fd.get(), &c, 1, 0));
    EXPECT_EQ('z', c);
    errno = 0;
    EXPECT_EQ(MAP_FAILED, mmap(nullptr, kPage, PROT_WRITE, MAP_SHARED, fd.get(), 0));
    EXPECT_EQ(EPERM, errno);
    void* ro = mmap(nullptr, kPage, PROT_READ, MAP_SHARED, fd.get(), 0);
    ASSERT_NE(MAP_FAILED, ro) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, mprotect(ro, kPage, PROT_READ | PROT_WRITE));
    EXPECT_EQ(EACCES, errno);
    ASSERT_EQ(0, munmap(ro, kPage));
    ASSERT_EQ(0, munmap(old, kPage));
    ASSERT_EQ(0, fcntl(fd.get(), F_ADD_SEALS, F_SEAL_WRITE));
}

TEST(Memfd, FixedMapFailurePreservesOldMapping) {
    Fd fd(Memfd("fixed", MFD_ALLOW_SEALING));
    ASSERT_GE(fd.get(), 0);
    ASSERT_EQ(0, ftruncate(fd.get(), kPage));
    void* old = mmap(nullptr, kPage, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, old);
    static_cast<char*>(old)[0] = 'q';
    ASSERT_EQ(0, fcntl(fd.get(), F_ADD_SEALS, F_SEAL_WRITE));
    errno = 0;
    EXPECT_EQ(MAP_FAILED, mmap(old, kPage, PROT_WRITE,
                               MAP_FIXED | MAP_SHARED, fd.get(), 0));
    EXPECT_EQ(EPERM, errno);
    EXPECT_EQ('q', static_cast<char*>(old)[0]);
    ASSERT_EQ(0, munmap(old, kPage));
}

TEST(Memfd, FallocateKeepSizeAndPunchHole) {
    Fd fd(Memfd("holes", MFD_ALLOW_SEALING));
    ASSERT_GE(fd.get(), 0);
    ASSERT_EQ(0, ftruncate(fd.get(), 2 * kPage));
    char data[32];
    memset(data, 'X', sizeof(data));
    ASSERT_EQ(static_cast<ssize_t>(sizeof(data)), pwrite(fd.get(), data, sizeof(data), kPage - 16));
    void* map = mmap(nullptr, 2 * kPage, PROT_READ, MAP_SHARED, fd.get(), 0);
    ASSERT_NE(MAP_FAILED, map);
    ASSERT_EQ(0, fallocate(fd.get(), FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE,
                            kPage - 8, 16));
    char observed[32] = {};
    ASSERT_EQ(static_cast<ssize_t>(sizeof(observed)),
              pread(fd.get(), observed, sizeof(observed), kPage - 16));
    for (int i = 0; i < 32; ++i) EXPECT_EQ(i >= 8 && i < 24 ? 0 : 'X', observed[i]);
    EXPECT_EQ(0, static_cast<const char*>(map)[kPage - 1]);
    ASSERT_EQ(0, munmap(map, 2 * kPage));
    struct stat st {};
    ASSERT_EQ(0, fstat(fd.get(), &st));
    EXPECT_EQ(static_cast<off_t>(2 * kPage), st.st_size);

    ASSERT_EQ(0, fallocate(fd.get(), FALLOC_FL_KEEP_SIZE, 3 * kPage, kPage));
    ASSERT_EQ(0, fstat(fd.get(), &st));
    EXPECT_EQ(static_cast<off_t>(2 * kPage), st.st_size);
    ASSERT_EQ(0, fcntl(fd.get(), F_ADD_SEALS, F_SEAL_WRITE));
    EXPECT_EQ(0, fallocate(fd.get(), 0, 0, kPage));
    errno = 0;
    EXPECT_EQ(-1, fallocate(fd.get(), FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0, kPage));
    EXPECT_EQ(EPERM, errno);
}

TEST(Memfd, OtherFilesAndTmpfs) {
    Fd ordinary(open("/etc/passwd", O_RDONLY));
    ASSERT_GE(ordinary.get(), 0);
    errno = 0;
    EXPECT_EQ(-1, fcntl(ordinary.get(), F_GET_SEALS));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, fcntl(ordinary.get(), F_ADD_SEALS, 0));
    EXPECT_EQ(EPERM, errno);
}

TEST(Memfd, ExecutableSealAndMode) {
    Fd fd(Memfd("exec", MFD_ALLOW_SEALING));
    ASSERT_GE(fd.get(), 0);
    ASSERT_EQ(0, ftruncate(fd.get(), kPage));
    void* old = mmap(nullptr, kPage, PROT_READ | PROT_WRITE, MAP_SHARED, fd.get(), 0);
    ASSERT_NE(MAP_FAILED, old);
    // Linux adds the implied WRITE/GROW/SHRINK bits after the explicit
    // WRITE mapping check; an existing mapping stays writable.
    ASSERT_EQ(0, fcntl(fd.get(), F_ADD_SEALS, F_SEAL_EXEC));
    EXPECT_EQ(F_SEAL_EXEC | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE |
                  F_SEAL_GROW | F_SEAL_SHRINK,
              fcntl(fd.get(), F_GET_SEALS));
    static_cast<char*>(old)[0] = 'e';
    char c = 0;
    ASSERT_EQ(1, pread(fd.get(), &c, 1, 0));
    EXPECT_EQ('e', c);
    errno = 0;
    EXPECT_EQ(-1, fchmod(fd.get(), 0666));
    EXPECT_EQ(EPERM, errno);
    ASSERT_EQ(0, munmap(old, kPage));

    Fd noexec(Memfd("noexec", MFD_NOEXEC_SEAL));
    ASSERT_GE(noexec.get(), 0);
    errno = 0;
    EXPECT_EQ(-1, fchmod(noexec.get(), 0777));
    EXPECT_EQ(EPERM, errno);
}

TEST(Memfd, SplitAndForkKeepWriterCount) {
    Fd fd(Memfd("split", MFD_ALLOW_SEALING));
    ASSERT_GE(fd.get(), 0);
    ASSERT_EQ(0, ftruncate(fd.get(), 3 * kPage));
    char* map = static_cast<char*>(mmap(nullptr, 3 * kPage,
        PROT_READ | PROT_WRITE, MAP_SHARED, fd.get(), 0));
    ASSERT_NE(MAP_FAILED, map);
    ASSERT_EQ(0, munmap(map + kPage, kPage));
    ASSERT_EQ(0, munmap(map, kPage));
    errno = 0;
    EXPECT_EQ(-1, fcntl(fd.get(), F_ADD_SEALS, F_SEAL_WRITE));
    EXPECT_EQ(EBUSY, errno);
    ASSERT_EQ(0, munmap(map + 2 * kPage, kPage));

    char* again = static_cast<char*>(mmap(nullptr, kPage,
        PROT_READ | PROT_WRITE, MAP_SHARED, fd.get(), 0));
    ASSERT_NE(MAP_FAILED, again);
    int sync_pipe[2];
    ASSERT_EQ(0, pipe(sync_pipe));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(sync_pipe[1]);
        char token = 0;
        if (read(sync_pipe[0], &token, 1) != 1) _exit(2);
        _exit(again[0] == 0 ? 0 : 3);
    }
    close(sync_pipe[0]);
    ASSERT_EQ(0, munmap(again, kPage));
    errno = 0;
    EXPECT_EQ(-1, fcntl(fd.get(), F_ADD_SEALS, F_SEAL_WRITE));
    EXPECT_EQ(EBUSY, errno);
    ASSERT_EQ(1, write(sync_pipe[1], "x", 1));
    close(sync_pipe[1]);
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_EQ(0, fcntl(fd.get(), F_ADD_SEALS, F_SEAL_WRITE));
}

TEST(Memfd, WholePagePunchVisibleToExistingMap) {
    Fd fd(Memfd("whole-hole", MFD_ALLOW_SEALING));
    ASSERT_GE(fd.get(), 0);
    ASSERT_EQ(0, ftruncate(fd.get(), 3 * kPage));
    ASSERT_EQ(1, pwrite(fd.get(), "Q", 1, kPage));
    char* map = static_cast<char*>(mmap(nullptr, 3 * kPage,
        PROT_READ, MAP_SHARED, fd.get(), 0));
    ASSERT_NE(MAP_FAILED, map);
    EXPECT_EQ('Q', map[kPage]);
    ASSERT_EQ(0, fallocate(fd.get(), FALLOC_FL_KEEP_SIZE | FALLOC_FL_PUNCH_HOLE,
                            kPage, kPage));
    EXPECT_EQ(0, map[kPage]);
    char c = 'X';
    ASSERT_EQ(1, pread(fd.get(), &c, 1, kPage));
    EXPECT_EQ(0, c);
    ASSERT_EQ(0, munmap(map, 3 * kPage));
}
}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
