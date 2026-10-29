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

// Reopen after unmount so a new inode/PageCache must fill the file from the
// read-only filesystem, rather than reusing pages dirtied by test setup.
class Ext4ReadOnlyRead : public ::testing::Test {
protected:
    dunitest::LoopExt4 fs;
    int fd = -1;
    std::vector<unsigned char> expected;

    void SetUp() override {
        ASSERT_NO_FATAL_FAILURE(fs.SetUp("ext4_inode_identity.img", "/tmp", "ext4_ro_read"));
        ASSERT_NO_FATAL_FAILURE(fs.Mount());
        const std::string path = fs.mount_point() + "/sparse-read";
        fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600);
        ASSERT_GE(fd, 0) << strerror(errno);
        expected.resize(5 * kPage + 137, 0);
        for (size_t i = 0; i < kPage; ++i) expected[i] = (i * 17 + 1) % 251;
        std::fill(expected.begin() + 5 * kPage, expected.end(), 0x6b);
        ASSERT_EQ(pwrite(fd, expected.data(), kPage, 0), static_cast<ssize_t>(kPage));
        ASSERT_EQ(pwrite(fd, expected.data() + 5 * kPage, 137, 5 * kPage), 137);
        ASSERT_EQ(fsync(fd), 0);
        ASSERT_EQ(close(fd), 0);
        fd = -1;
        ASSERT_NO_FATAL_FAILURE(fs.Unmount());
        ASSERT_NO_FATAL_FAILURE(fs.Mount(MS_RDONLY));
        fd = open(path.c_str(), O_RDONLY);
        ASSERT_GE(fd, 0) << strerror(errno);
    }

    void TearDown() override {
        if (fd >= 0) close(fd);
    }
};

TEST_F(Ext4ReadOnlyRead, ColdMmapReadsDataHolesAndZeroesEofTail) {
    const size_t mapped_size = 6 * kPage;
    void* mapping = mmap(nullptr, mapped_size, PROT_READ, MAP_PRIVATE, fd, 0);
    ASSERT_NE(mapping, MAP_FAILED) << strerror(errno);
    EXPECT_EQ(memcmp(mapping, expected.data(), expected.size()), 0);
    const auto* bytes = static_cast<const unsigned char*>(mapping);
    EXPECT_TRUE(std::all_of(bytes + expected.size(), bytes + mapped_size,
                            [](unsigned char value) { return value == 0; }));
    EXPECT_EQ(munmap(mapping, mapped_size), 0);
}

TEST_F(Ext4ReadOnlyRead, ColdOffsetMappingAndRandomAdvice) {
    // Begin inside the file and fault at a non-page-aligned byte address. The
    // final partial page and holes must survive both normal and random advice.
    for (int advice : {MADV_NORMAL, MADV_RANDOM}) {
        if (advice == MADV_RANDOM) {
            ASSERT_EQ(close(fd), 0);
            fd = -1;
            ASSERT_NO_FATAL_FAILURE(fs.Unmount());
            ASSERT_NO_FATAL_FAILURE(fs.Mount(MS_RDONLY));
            fd = open((fs.mount_point() + "/sparse-read").c_str(), O_RDONLY);
            ASSERT_GE(fd, 0) << strerror(errno);
        }
        const size_t length = 5 * kPage;
        void* mapping = mmap(nullptr, length, PROT_READ, MAP_PRIVATE, fd, kPage);
        ASSERT_NE(mapping, MAP_FAILED) << strerror(errno);
        ASSERT_EQ(madvise(mapping, length, advice), 0) << strerror(errno);
        const auto* bytes = static_cast<const unsigned char*>(mapping);
        EXPECT_EQ(bytes[4 * kPage + 17], expected[5 * kPage + 17]);
        EXPECT_EQ(memcmp(bytes, expected.data() + kPage, expected.size() - kPage), 0);
        EXPECT_EQ(bytes[length - 1], 0);
        ASSERT_EQ(munmap(mapping, length), 0);
    }
}

TEST(Ext4ColdMmap, SharedWriteRetriesBeforePageMkwrite) {
    dunitest::LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp("ext4_inode_identity.img", "/tmp", "ext4_cold_shared"));
    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    const std::string path = fs.mount_point() + "/cold-shared";
    int fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600);
    ASSERT_GE(fd, 0) << strerror(errno);
    const size_t length = 20 * kPage;
    std::vector<unsigned char> expected(length);
    for (size_t i = 0; i < length; ++i) expected[i] = (i * 13 + i / kPage) % 251;
    ASSERT_EQ(write(fd, expected.data(), length), static_cast<ssize_t>(length));
    ASSERT_EQ(fsync(fd), 0);
    ASSERT_EQ(close(fd), 0);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    fd = open(path.c_str(), O_RDWR);
    ASSERT_GE(fd, 0) << strerror(errno);
    void* mapping = mmap(nullptr, length, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    ASSERT_NE(mapping, MAP_FAILED) << strerror(errno);
    auto* bytes = static_cast<unsigned char*>(mapping);
    // Store first: a read must not warm the demanded page before shared fault.
    bytes[37] = 0x9d;
    expected[37] = 0x9d;
    EXPECT_EQ(memcmp(bytes, expected.data(), length), 0);
    ASSERT_EQ(msync(mapping, length, MS_SYNC), 0) << strerror(errno);
    ASSERT_EQ(munmap(mapping, length), 0);
    std::vector<unsigned char> output(length);
    EXPECT_EQ(pread(fd, output.data(), length, 0), static_cast<ssize_t>(length));
    EXPECT_EQ(output, expected);
    EXPECT_EQ(close(fd), 0);
}

TEST_F(Ext4ReadOnlyRead, ColdBufferedReadClipsAtEof) {
    std::vector<unsigned char> output(expected.size() + kPage, 0xa5);
    EXPECT_EQ(pread(fd, output.data(), output.size(), 0),
              static_cast<ssize_t>(expected.size()));
    EXPECT_EQ(memcmp(output.data(), expected.data(), expected.size()), 0);
    EXPECT_EQ(output[expected.size()], 0xa5);
    EXPECT_EQ(pread(fd, output.data(), output.size(), expected.size()), 0);
}


TEST_F(Ext4ReadOnlyRead, WritableOpenRejectedAndPrivateCowRemainsPrivate) {
    errno = 0;
    const int writer = open((fs.mount_point() + "/sparse-read").c_str(), O_RDWR);
    const int open_error = errno;
    if (writer >= 0) close(writer);
    EXPECT_EQ(writer, -1);
    EXPECT_EQ(open_error, EROFS);
    void* mapping = mmap(nullptr, kPage, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    ASSERT_NE(mapping, MAP_FAILED) << strerror(errno);
    auto* bytes = static_cast<unsigned char*>(mapping);
    EXPECT_EQ(bytes[0], expected[0]);
    bytes[0] ^= 0xff;
    unsigned char original = 0;
    EXPECT_EQ(pread(fd, &original, 1, 0), 1);
    EXPECT_EQ(original, expected[0]);
    EXPECT_EQ(munmap(mapping, kPage), 0);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
