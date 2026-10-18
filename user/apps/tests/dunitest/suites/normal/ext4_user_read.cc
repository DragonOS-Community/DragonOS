#include <gtest/gtest.h>
#include "loop_ext4_test_support.h"

#include <fcntl.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/statfs.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <string>
#include <vector>

namespace {
constexpr size_t kPage = 4096;
constexpr size_t kSize = 80 * kPage;
constexpr long kExt4Magic = 0xef53;

class Ext4UserRead : public ::testing::Test {
protected:
    int fd = -1;
    std::vector<unsigned char> data;
    std::unique_ptr<dunitest::LoopExt4> loop;
    std::string directory;

    void SetUp() override {
        const char* explicit_directory = std::getenv("EXT4_USER_READ_DIR");
        if (explicit_directory != nullptr) {
            directory = explicit_directory;
        } else {
            loop = std::make_unique<dunitest::LoopExt4>();
            ASSERT_NO_FATAL_FAILURE(loop->SetUp("ext4_inode_identity.img", "/tmp", "ext4_user_read"));
            ASSERT_NO_FATAL_FAILURE(loop->Mount());
            directory = loop->mount_point();
        }
        std::string path = directory + "/ext4-user-read-XXXXXX";
        fd = mkstemp(path.data());
        ASSERT_GE(fd, 0) << strerror(errno);
        ASSERT_EQ(unlink(path.c_str()), 0);
        struct statfs info {};
        ASSERT_EQ(fstatfs(fd, &info), 0);
        ASSERT_EQ(info.f_type, kExt4Magic) << "requires a real ext4 test directory";
        data.resize(kSize);
        for (size_t i = 0; i < data.size(); ++i) data[i] = (i * 17 + i / kPage) % 251;
        ASSERT_EQ(write(fd, data.data(), data.size()), static_cast<ssize_t>(data.size()));
        ASSERT_EQ(fsync(fd), 0);
        ASSERT_EQ(lseek(fd, 0, SEEK_SET), 0);
    }

    void TearDown() override {
        if (fd >= 0) close(fd);
    }

    // The file was unlinked before fork. On timeout the parent neither waits
    // indefinitely nor touches the inode held by the possibly blocked child.
    void SelfMapped(int operation, bool shared) {
        const pid_t child = fork();
        ASSERT_GE(child, 0);
        if (child == 0) {
            void* target = mmap(nullptr, kPage, PROT_READ | PROT_WRITE,
                                shared ? MAP_SHARED : MAP_PRIVATE, fd, 0);
            if (target == MAP_FAILED) _exit(11);
            if (!shared) {
                const volatile unsigned char first = *static_cast<unsigned char*>(target);
                if (first != data[0]) _exit(12);
            }
            ssize_t count;
            if (operation == 0) {
                count = read(fd, target, kPage);
            } else if (operation == 1) {
                count = pread(fd, target, kPage, 0);
            } else {
                iovec vectors[] = {{target, 123},
                    {static_cast<unsigned char*>(target) + 123, kPage - 123}};
                count = readv(fd, vectors, 2);
            }
            if (count != static_cast<ssize_t>(kPage)) _exit(13);
            if (memcmp(target, data.data(), kPage) != 0) _exit(14);
            if (lseek(fd, 0, SEEK_CUR) != (operation == 1 ? 0 : static_cast<off_t>(kPage)))
                _exit(15);
            munmap(target, kPage);
            _exit(0);
        }
        close(fd);
        fd = -1;
        for (int attempt = 0; attempt < 400; ++attempt) {
            int status = 0;
            const pid_t result = waitpid(child, &status, WNOHANG);
            if (result == child) {
                ASSERT_TRUE(WIFEXITED(status));
                ASSERT_EQ(WEXITSTATUS(status), 0);
                return;
            }
            ASSERT_EQ(result, 0) << strerror(errno);
            usleep(25000);
        }
        kill(child, SIGKILL);
        int status = 0;
        (void)waitpid(child, &status, WNOHANG);
        // A task stuck inside a filesystem operation can prevent unmount.
        // Preserve that failed fixture for VM disposal instead of blocking the
        // test runner in LoopExt4's destructor.
        (void)loop.release();
        FAIL() << "self-mapped read exceeded 10 seconds (possible source-page fault lock cycle)";
    }
};

TEST_F(Ext4UserRead, SelfMappedSharedRead) { SelfMapped(0, true); }
TEST_F(Ext4UserRead, SelfMappedSharedPread) { SelfMapped(1, true); }
TEST_F(Ext4UserRead, SelfMappedSharedReadv) { SelfMapped(2, true); }
TEST_F(Ext4UserRead, SelfMappedPrivateCow) { SelfMapped(1, false); }

TEST_F(Ext4UserRead, FaultAfterFirstPagePreservesPrefixAndPosition) {
    void* target = mmap(nullptr, 2 * kPage, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(target, MAP_FAILED);
    ASSERT_EQ(mprotect(static_cast<unsigned char*>(target) + kPage, kPage, PROT_NONE), 0);
    EXPECT_EQ(read(fd, target, 2 * kPage), static_cast<ssize_t>(kPage));
    EXPECT_EQ(lseek(fd, 0, SEEK_CUR), static_cast<off_t>(kPage));
    EXPECT_EQ(memcmp(target, data.data(), kPage), 0);
    ASSERT_EQ(lseek(fd, 0, SEEK_SET), 0);
    EXPECT_EQ(pread(fd, target, 2 * kPage, 0), static_cast<ssize_t>(kPage));
    EXPECT_EQ(lseek(fd, 0, SEEK_CUR), 0);
    iovec vectors[] = {{target, kPage},
        {static_cast<unsigned char*>(target) + kPage, kPage}};
    EXPECT_EQ(readv(fd, vectors, 2), static_cast<ssize_t>(kPage));
    EXPECT_EQ(lseek(fd, 0, SEEK_CUR), static_cast<off_t>(kPage));
    munmap(target, 2 * kPage);
}

TEST_F(Ext4UserRead, InitialFaultDoesNotAdvancePositionOrSkipIovec) {
    void* inaccessible = mmap(nullptr, kPage, PROT_NONE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(inaccessible, MAP_FAILED);
    errno = 0;
    EXPECT_EQ(read(fd, inaccessible, kPage), -1);
    EXPECT_EQ(errno, EFAULT);
    EXPECT_EQ(lseek(fd, 0, SEEK_CUR), 0);
    std::array<unsigned char, kPage> untouched;
    untouched.fill(0xa5);
    iovec vectors[] = {{nullptr, 0}, {inaccessible, kPage},
                       {untouched.data(), untouched.size()}};
    errno = 0;
    EXPECT_EQ(readv(fd, vectors, 3), -1);
    EXPECT_EQ(errno, EFAULT);
    EXPECT_EQ(lseek(fd, 0, SEEK_CUR), 0);
    EXPECT_TRUE(std::all_of(untouched.begin(), untouched.end(),
                           [](unsigned char byte) { return byte == 0xa5; }));
    munmap(inaccessible, kPage);
}

TEST_F(Ext4UserRead, DirectoryRetainsEisdirDispatch) {
    const int directory_fd = open(directory.c_str(), O_RDONLY | O_DIRECTORY);
    ASSERT_GE(directory_fd, 0);
    unsigned char output = 0;
    errno = 0;
    EXPECT_EQ(read(directory_fd, &output, 1), -1);
    EXPECT_EQ(errno, EISDIR);
    iovec vector = {&output, 1};
    errno = 0;
    EXPECT_EQ(readv(directory_fd, &vector, 1), -1);
    EXPECT_EQ(errno, EISDIR);
    errno = 0;
    EXPECT_EQ(pread(directory_fd, &output, 1, 0), -1);
    EXPECT_EQ(errno, EISDIR);
    close(directory_fd);
}

TEST_F(Ext4UserRead, LargeReadAndManyIovecsPreserveOrder) {
    // Evict clean pages where supported; correctness must also hold if the
    // advisory operation is ignored and every page remains warm.
    (void)posix_fadvise(fd, 0, data.size(), POSIX_FADV_DONTNEED);
    std::vector<unsigned char> output(data.size(), 0);
    ASSERT_EQ(read(fd, output.data(), output.size()), static_cast<ssize_t>(output.size()));
    EXPECT_EQ(output, data);
    ASSERT_EQ(lseek(fd, 0, SEEK_SET), 0);
    std::fill(output.begin(), output.end(), 0);
    std::array<iovec, 81> vectors {};
    vectors[0] = {nullptr, 0};
    for (size_t i = 0; i < 80; ++i) vectors[i + 1] = {output.data() + i * kPage, kPage};
    ASSERT_EQ(readv(fd, vectors.data(), vectors.size()), static_cast<ssize_t>(output.size()));
    EXPECT_EQ(output, data);
    EXPECT_EQ(lseek(fd, 0, SEEK_CUR), static_cast<off_t>(data.size()));
}

TEST_F(Ext4UserRead, EofAndZeroCountDoNotTouchDestination) {
    void* inaccessible = mmap(nullptr, kPage, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(inaccessible, MAP_FAILED);
    EXPECT_EQ(pread(fd, inaccessible, kPage, data.size()), 0);
    EXPECT_EQ(read(fd, inaccessible, 0), 0);
    EXPECT_EQ(lseek(fd, 0, SEEK_CUR), 0);
    iovec empty[] = {{inaccessible, 0}};
    EXPECT_EQ(readv(fd, empty, 1), 0);
    EXPECT_EQ(lseek(fd, 0, SEEK_CUR), 0);
    munmap(inaccessible, kPage);
}

TEST_F(Ext4UserRead, AlignedDirectReadSmoke) {
    std::string path = directory + "/ext4-user-direct-XXXXXX";
    const int writer = mkstemp(path.data());
    ASSERT_GE(writer, 0);
    std::array<unsigned char, kPage> expected {};
    for (size_t i = 0; i < expected.size(); ++i) expected[i] = i % 251;
    ASSERT_EQ(write(writer, expected.data(), expected.size()), static_cast<ssize_t>(kPage));
    ASSERT_EQ(fsync(writer), 0);
    const int reader = open(path.c_str(), O_RDONLY | O_DIRECT);
    const int open_error = errno;
    unlink(path.c_str());
    close(writer);
    ASSERT_GE(reader, 0) << strerror(open_error);
    void* output = nullptr;
    ASSERT_EQ(posix_memalign(&output, kPage, kPage), 0);
    EXPECT_EQ(read(reader, output, kPage), static_cast<ssize_t>(kPage));
    EXPECT_EQ(memcmp(output, expected.data(), kPage), 0);
    EXPECT_EQ(lseek(reader, 0, SEEK_CUR), static_cast<off_t>(kPage));
    free(output);
    close(reader);
}
}  // namespace
