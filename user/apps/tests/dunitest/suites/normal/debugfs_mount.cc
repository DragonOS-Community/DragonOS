#include <gtest/gtest.h>

#include <errno.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>

namespace {

int ensure_dir(const char* path) {
    struct stat st = {};
    if (stat(path, &st) == 0) {
        return S_ISDIR(st.st_mode) ? 0 : -1;
    }
    return mkdir(path, 0755);
}

TEST(DebugFsMount, MountCreatesDebugFilesystem) {
    char root[128] = {};
    snprintf(root, sizeof(root), "/tmp/debugfs_mount_%d", getpid());

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, mkdir(root, 0755)) << strerror(errno);

    if (mount("none", root, "debugfs", 0, nullptr) != 0) {
        int saved_errno = errno;
        rmdir(root);
        FAIL() << "mount debugfs failed: errno=" << saved_errno << " (" << strerror(saved_errno)
               << ")";
    }

    struct stat st = {};
    ASSERT_EQ(0, stat(root, &st)) << strerror(errno);
    EXPECT_TRUE(S_ISDIR(st.st_mode));

    ASSERT_EQ(0, umount(root)) << strerror(errno);
    ASSERT_EQ(0, rmdir(root)) << strerror(errno);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
