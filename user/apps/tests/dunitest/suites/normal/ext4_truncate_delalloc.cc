#include <gtest/gtest.h>

#include "loop_ext4_test_support.h"

#include <fcntl.h>
#include <signal.h>
#include <sys/stat.h>
#include <unistd.h>

#include <string>
#include <vector>

namespace {

// An outer runner deadline remains necessary for an uninterruptible kernel
// wait; the process alarm also bounds ordinary blocking syscall failures.
class Deadline {
  public:
    Deadline() { alarm(180); }
    ~Deadline() { alarm(0); }
};

class Ext4TruncateDelalloc : public ::testing::TestWithParam<int> {};

TEST_P(Ext4TruncateDelalloc, TruncateAfterTwoLargeBufferedFiles) {
    Deadline deadline;
    dunitest::LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp("ext4_truncate_delalloc.img"));
    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    const int mode = GetParam(); // 0: dirty, 1: fsync, 2: fresh remount.
    std::vector<char> zeros(1024 * 1024, 0);
    const std::string target = fs.mount_point() + "/test_file";
    for (const char* name : {"zero_file", "test_file"}) {
        const std::string path = fs.mount_point() + "/" + name;
        const int fd = open(path.c_str(), O_CREAT | O_EXCL | O_WRONLY, 0600);
        ASSERT_GE(fd, 0) << strerror(errno);
        for (int block = 0; block < 64; ++block) {
            ASSERT_EQ(write(fd, zeros.data(), zeros.size()),
                      static_cast<ssize_t>(zeros.size())) << strerror(errno);
        }
        if (mode == 1) {
            ASSERT_EQ(fsync(fd), 0) << strerror(errno);
        }
        ASSERT_EQ(close(fd), 0) << strerror(errno);
    }
    if (mode == 2) {
        ASSERT_NO_FATAL_FAILURE(fs.Unmount());
        ASSERT_NO_FATAL_FAILURE(fs.Mount());
    }

    struct stat before = {};
    ASSERT_EQ(stat(target.c_str(), &before), 0);
    ASSERT_EQ(before.st_size, 64 * 1024 * 1024);
    int fd = open(target.c_str(), O_WRONLY | O_CLOEXEC | O_NOFOLLOW);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(close(fd), 0);

    // The dirty mode deliberately reaches this call without fsync or remount.
    fd = open(target.c_str(), O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC | O_NOFOLLOW, 0600);
    ASSERT_GE(fd, 0) << "mode=" << mode << ": " << strerror(errno);
    struct stat after = {};
    ASSERT_EQ(fstat(fd, &after), 0);
    EXPECT_EQ(after.st_size, 0);
    EXPECT_EQ(after.st_ino, before.st_ino);
    const char marker[] = "after-truncate";
    ASSERT_EQ(write(fd, marker, sizeof(marker)), static_cast<ssize_t>(sizeof(marker)));
    ASSERT_EQ(fsync(fd), 0);
    ASSERT_EQ(close(fd), 0);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    fd = open(target.c_str(), O_RDONLY);
    ASSERT_GE(fd, 0);
    ASSERT_EQ(fstat(fd, &after), 0);
    EXPECT_EQ(after.st_size, static_cast<off_t>(sizeof(marker)));
    char observed[sizeof(marker)] = {};
    EXPECT_EQ(read(fd, observed, sizeof(observed)), static_cast<ssize_t>(sizeof(observed)));
    EXPECT_EQ(memcmp(observed, marker, sizeof(marker)), 0);
    ASSERT_EQ(close(fd), 0);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

INSTANTIATE_TEST_SUITE_P(DirtySyncedRemounted, Ext4TruncateDelalloc,
                         ::testing::Values(0, 1, 2));

} // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
