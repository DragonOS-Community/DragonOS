#include <gtest/gtest.h>

#include <array>
#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <string>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

namespace {

TEST(ReadlinkatEmptyPath, ProcThreadSelfViaPathFd) {
    const int fd = open("/proc/thread-self", O_PATH | O_NOFOLLOW | O_CLOEXEC);
    ASSERT_GE(fd, 0) << std::strerror(errno);

    std::array<char, 128> by_fd {};
    std::array<char, 128> by_path {};
    const ssize_t fd_len = readlinkat(fd, "", by_fd.data(), by_fd.size());
    const ssize_t path_len = readlink("/proc/thread-self", by_path.data(), by_path.size());
    ASSERT_GT(fd_len, 0) << std::strerror(errno);
    ASSERT_EQ(fd_len, path_len);
    EXPECT_EQ(std::string(by_path.data(), path_len), std::string(by_fd.data(), fd_len));

    char byte = 0;
    errno = 0;
    EXPECT_EQ(-1, read(fd, &byte, 1));
    EXPECT_EQ(EBADF, errno);
    EXPECT_EQ(0, close(fd));
}

TEST(ReadlinkatEmptyPath, FilesystemLinkAndPathWhitespace) {
    char directory[] = "/tmp/dkc008_readlink_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(directory)) << std::strerror(errno);
    const std::string link = std::string(directory) + "/link";
    const std::string spaced_link = std::string(directory) + "/ padded ";
    const std::string cleanup_link = std::string(directory) + "/cleanup";
    ASSERT_EQ(0, symlink("target-value", link.c_str())) << std::strerror(errno);
    ASSERT_EQ(0, symlink("target-value", spaced_link.c_str())) << std::strerror(errno);

    const int fd = open(link.c_str(), O_PATH | O_NOFOLLOW | O_CLOEXEC);
    ASSERT_GE(fd, 0) << std::strerror(errno);

    std::array<char, 32> full {};
    EXPECT_EQ(12, readlinkat(fd, "", full.data(), full.size()));
    EXPECT_EQ("target-value", std::string(full.data(), 12));
    full.fill(0);
    EXPECT_EQ(12, readlinkat(AT_FDCWD, spaced_link.c_str(), full.data(), full.size()));
    EXPECT_EQ("target-value", std::string(full.data(), 12));

    std::array<char, 3> short_buf {};
    EXPECT_EQ(3, readlinkat(fd, "", short_buf.data(), short_buf.size()));
    EXPECT_EQ("tar", std::string(short_buf.data(), short_buf.size()));

    EXPECT_EQ(0, close(fd));
    EXPECT_EQ(0, unlink(link.c_str()));
    // DragonOS unlink currently trims trailing spaces (a separate VFS issue).
    // Rename the entry to a plain name so this readlinkat test can clean up.
    EXPECT_EQ(0, rename(spaced_link.c_str(), cleanup_link.c_str()));
    EXPECT_EQ(0, unlink(cleanup_link.c_str()));
    EXPECT_EQ(0, rmdir(directory));
}

TEST(ReadlinkatEmptyPath, EmptyPathErrorsAndPriority) {
    std::array<char, 32> buffer {};
    const int directory = open("/proc", O_PATH | O_CLOEXEC);
    ASSERT_GE(directory, 0) << std::strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, readlinkat(directory, "", buffer.data(), buffer.size()));
    EXPECT_EQ(ENOENT, errno);
    errno = 0;
    EXPECT_EQ(-1, readlinkat(-1, "", buffer.data(), buffer.size()));
    EXPECT_EQ(EBADF, errno);
    errno = 0;
    EXPECT_EQ(-1, readlinkat(AT_FDCWD, "", buffer.data(), buffer.size()));
    EXPECT_EQ(ENOENT, errno);
    errno = 0;
    EXPECT_EQ(-1, readlinkat(AT_FDCWD, "/proc", buffer.data(), buffer.size()));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, readlinkat(directory, "", buffer.data(), 0));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_readlinkat, -1, "", nullptr, 1));
    EXPECT_EQ(EBADF, errno);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_readlinkat, directory, "", nullptr, 1));
    EXPECT_EQ(ENOENT, errno);

    EXPECT_EQ(0, close(directory));
}

TEST(ReadlinkatEmptyPath, BadUserBufferReturnsEfault) {
    const int fd = open("/proc/thread-self", O_PATH | O_NOFOLLOW | O_CLOEXEC);
    ASSERT_GE(fd, 0) << std::strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_readlinkat, fd, "", nullptr, 128));
    EXPECT_EQ(EFAULT, errno);

    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    void* page = mmap(nullptr, page_size, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, page) << std::strerror(errno);
    ASSERT_EQ(0, mprotect(page, page_size, PROT_READ));
    errno = 0;
    EXPECT_EQ(-1, readlinkat(fd, "", static_cast<char*>(page), 128));
    EXPECT_EQ(EFAULT, errno);
    ASSERT_EQ(0, munmap(page, page_size));
    errno = 0;
    EXPECT_EQ(-1, readlinkat(fd, "", static_cast<char*>(page), 128));
    EXPECT_EQ(EFAULT, errno);

    EXPECT_EQ(0, close(fd));
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
