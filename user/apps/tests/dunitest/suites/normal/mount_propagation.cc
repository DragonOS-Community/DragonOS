#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>

#ifndef CLONE_NEWNS
#define CLONE_NEWNS 0x00020000
#endif

#ifndef MS_REC
#define MS_REC 16384
#endif

namespace {

int ensure_dir(const char* path) {
    struct stat st = {};
    if (stat(path, &st) == 0) {
        return S_ISDIR(st.st_mode) ? 0 : -1;
    }
    return mkdir(path, 0755);
}

bool path_exists(const char* path) {
    struct stat st = {};
    return stat(path, &st) == 0;
}

int create_marker(const char* mount_point, const char* name) {
    char path[256] = {};
    snprintf(path, sizeof(path), "%s/%s", mount_point, name);

    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        return -1;
    }
    const int ret = write(fd, "x", 1);
    const int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return ret == 1 ? 0 : -1;
}

bool marker_exists(const char* mount_point, const char* name) {
    char path[256] = {};
    snprintf(path, sizeof(path), "%s/%s", mount_point, name);
    return path_exists(path);
}

void best_effort_umount(const char* path) {
    if (umount(path) != 0 && errno != EINVAL && errno != ENOENT) {
        ADD_FAILURE() << "umount failed for " << path << ": errno=" << errno << " ("
                      << strerror(errno) << ")";
    }
}

void best_effort_rmdir(const char* path) {
    if (rmdir(path) != 0 && errno != ENOENT && errno != ENOTEMPTY) {
        ADD_FAILURE() << "rmdir failed for " << path << ": errno=" << errno << " ("
                      << strerror(errno) << ")";
    }
}

void cleanup_path(const char* path) {
    umount(path);
    rmdir(path);
}

class MountPropagationTest : public ::testing::Test {
protected:
    char root_[128] = {};

    void SetUp() override {
        ASSERT_EQ(0, ensure_dir("/tmp")) << strerror(errno);
        snprintf(root_, sizeof(root_), "/tmp/mount_propagation_%d", getpid());
        ASSERT_EQ(0, ensure_dir(root_)) << strerror(errno);

        if (unshare(CLONE_NEWNS) != 0) {
            GTEST_SKIP() << "unshare(CLONE_NEWNS): " << strerror(errno);
        }
        ASSERT_EQ(0, mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr))
            << strerror(errno);
    }

    void TearDown() override {
        char path[192] = {};
        const char* suffixes[] = {
            "/slave_a/local", "/slave_b/local", "/slave/local", "/base/host", "/slave_a/host",
            "/slave_b/host",  "/slave/host",   "/slave_b",     "/slave_a",   "/slave",
            "/base",
        };

        for (const char* suffix : suffixes) {
            snprintf(path, sizeof(path), "%s%s", root_, suffix);
            cleanup_path(path);
        }
        best_effort_rmdir(root_);
    }
};

}  // namespace

TEST_F(MountPropagationTest, SlaveReceivesMasterPropagationOnly) {
    char base[160] = {};
    char slave[160] = {};
    char host_sub[192] = {};
    char slave_host_sub[192] = {};
    char slave_local[192] = {};
    char base_local[192] = {};

    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(slave, sizeof(slave), "%s/slave", root_);
    snprintf(host_sub, sizeof(host_sub), "%s/host", base);
    snprintf(slave_host_sub, sizeof(slave_host_sub), "%s/host", slave);
    snprintf(slave_local, sizeof(slave_local), "%s/local", slave);
    snprintf(base_local, sizeof(base_local), "%s/local", base);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(slave)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(base, slave, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, slave, nullptr, MS_SLAVE, nullptr)) << strerror(errno);

    ASSERT_EQ(0, ensure_dir(host_sub)) << strerror(errno);
    ASSERT_EQ(0, mount("", host_sub, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(host_sub, "host_marker")) << strerror(errno);
    EXPECT_TRUE(marker_exists(slave_host_sub, "host_marker"));

    ASSERT_EQ(0, ensure_dir(slave_local)) << strerror(errno);
    ASSERT_EQ(0, mount("", slave_local, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(slave_local, "slave_marker")) << strerror(errno);
    EXPECT_FALSE(marker_exists(base_local, "slave_marker"));

    best_effort_umount(slave_local);
    best_effort_rmdir(slave_local);
    best_effort_umount(host_sub);
    best_effort_rmdir(host_sub);
    best_effort_umount(slave);
    best_effort_rmdir(slave);
    best_effort_umount(base);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, SharedSlaveKeepsPeerPropagation) {
    char base[160] = {};
    char slave_a[160] = {};
    char slave_b[160] = {};
    char host_sub[192] = {};
    char slave_a_host[192] = {};
    char slave_b_host[192] = {};
    char slave_a_local[192] = {};
    char slave_b_local[192] = {};
    char base_local[192] = {};

    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(slave_a, sizeof(slave_a), "%s/slave_a", root_);
    snprintf(slave_b, sizeof(slave_b), "%s/slave_b", root_);
    snprintf(host_sub, sizeof(host_sub), "%s/host", base);
    snprintf(slave_a_host, sizeof(slave_a_host), "%s/host", slave_a);
    snprintf(slave_b_host, sizeof(slave_b_host), "%s/host", slave_b);
    snprintf(slave_a_local, sizeof(slave_a_local), "%s/local", slave_a);
    snprintf(slave_b_local, sizeof(slave_b_local), "%s/local", slave_b);
    snprintf(base_local, sizeof(base_local), "%s/local", base);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(slave_a)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(slave_b)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(base, slave_a, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, slave_a, nullptr, MS_SLAVE, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, slave_a, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(slave_a, slave_b, nullptr, MS_BIND, nullptr)) << strerror(errno);

    ASSERT_EQ(0, ensure_dir(host_sub)) << strerror(errno);
    ASSERT_EQ(0, mount("", host_sub, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(host_sub, "host_marker")) << strerror(errno);
    EXPECT_TRUE(marker_exists(slave_a_host, "host_marker"));
    EXPECT_TRUE(marker_exists(slave_b_host, "host_marker"));

    ASSERT_EQ(0, ensure_dir(slave_a_local)) << strerror(errno);
    ASSERT_EQ(0, mount("", slave_a_local, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(slave_a_local, "slave_marker")) << strerror(errno);
    EXPECT_TRUE(marker_exists(slave_b_local, "slave_marker"));
    EXPECT_FALSE(marker_exists(base_local, "slave_marker"));

    best_effort_umount(slave_a_local);
    best_effort_rmdir(slave_a_local);
    best_effort_umount(host_sub);
    best_effort_rmdir(host_sub);
    best_effort_umount(slave_b);
    best_effort_rmdir(slave_b);
    best_effort_umount(slave_a);
    best_effort_rmdir(slave_a);
    best_effort_umount(base);
    best_effort_rmdir(base);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
