#include <gtest/gtest.h>

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include "cap_common.h"

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

TEST(DebugFsMount, MountRequiresCapSysAdmin) {
    char root[128] = {};
    snprintf(root, sizeof(root), "/tmp/debugfs_mount_no_cap_%d", getpid());

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, mkdir(root, 0755)) << strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);

    if (child == 0) {
        cap_user_data_t zero[_LINUX_CAPABILITY_U32S_3] = {};
        if (capset_errno(_LINUX_CAPABILITY_VERSION_3, 0, zero) != 0) {
            _exit(10);
        }

        errno = 0;
        if (mount("none", root, "debugfs", 0, nullptr) == 0) {
            umount(root);
            _exit(11);
        }
        _exit(errno == EPERM ? 0 : 12);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);

    bool child_exited = WIFEXITED(status);
    int child_exit_code = child_exited ? WEXITSTATUS(status) : -1;
    if (child_exit_code == 11) {
        umount(root);
    }

    EXPECT_TRUE(child_exited) << "child terminated abnormally, status=" << status;
    EXPECT_EQ(0, child_exit_code)
        << "child expected debugfs mount to fail with EPERM after dropping caps";

    ASSERT_EQ(0, rmdir(root)) << strerror(errno);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
