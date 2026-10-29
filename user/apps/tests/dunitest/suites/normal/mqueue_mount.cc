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

TEST(MqueueMount, MountCreatesMqueueFilesystem) {
    char root[128] = {};
    snprintf(root, sizeof(root), "/tmp/mqueue_mount_%d", getpid());

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, mkdir(root, 0755)) << strerror(errno);

    if (mount("mqueue", root, "mqueue", 0, nullptr) != 0) {
        int saved_errno = errno;
        rmdir(root);
        FAIL() << "mount mqueue failed: errno=" << saved_errno << " (" << strerror(saved_errno)
               << ")";
    }

    struct stat st = {};
    ASSERT_EQ(0, stat(root, &st)) << strerror(errno);
    EXPECT_TRUE(S_ISDIR(st.st_mode));
    EXPECT_EQ(static_cast<mode_t>(01777), st.st_mode & static_cast<mode_t>(07777));

    ASSERT_EQ(0, umount(root)) << strerror(errno);
    ASSERT_EQ(0, rmdir(root)) << strerror(errno);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
