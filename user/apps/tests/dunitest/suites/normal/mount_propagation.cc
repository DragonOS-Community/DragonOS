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

#ifndef MS_SILENT
#define MS_SILENT 32768
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

int shared_group_id(const char* mount_point) {
    FILE* fp = fopen("/proc/self/mountinfo", "r");
    if (fp == nullptr) {
        return -1;
    }

    int result = -1;
    char line[1024] = {};
    while (fgets(line, sizeof(line), fp) != nullptr) {
        char parsed_mount_point[256] = {};
        if (sscanf(line, "%*s %*s %*s %*s %255s", parsed_mount_point) != 1 ||
            strcmp(parsed_mount_point, mount_point) != 0) {
            continue;
        }

        char* optional_end = strstr(line, " - ");
        char* shared = strstr(line, " shared:");
        if (shared != nullptr && optional_end != nullptr && shared < optional_end &&
            sscanf(shared, " shared:%d", &result) == 1) {
            break;
        }
        result = -1;
        break;
    }
    fclose(fp);
    return result;
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
            "/target_b/bind/host", "/target_a/bind/host", "/source/host",  "/master/host",
            "/src/host",           "/target_b/bind",      "/target_a/bind", "/target_b",
            "/target_a",           "/source",             "/master",       "/src",
            "/slave_a/local",      "/slave_b/local",      "/slave/local",  "/base/host",
            "/slave_a/host",       "/slave_b/host",       "/slave/host",   "/slave_b",
            "/slave_a",            "/slave",              "/base",
        };

        for (const char* suffix : suffixes) {
            snprintf(path, sizeof(path), "%s%s", root_, suffix);
            cleanup_path(path);
        }
        best_effort_rmdir(root_);
    }
};

}  // namespace

TEST_F(MountPropagationTest, PropagationFlagsAreStrictlyValidated) {
    char base[160] = {};
    snprintf(base, sizeof(base), "%s/base", root_);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, mount(nullptr, base, nullptr, MS_SHARED | MS_PRIVATE, nullptr));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(-1, shared_group_id(base));

    errno = 0;
    EXPECT_EQ(-1, mount(nullptr, base, nullptr, MS_SHARED | 0x200, nullptr));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(-1, shared_group_id(base));

    errno = 0;
    EXPECT_EQ(-1, mount(nullptr, base, nullptr, MS_SHARED | MS_NODEV, nullptr));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(-1, shared_group_id(base));

    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED | MS_REC | MS_SILENT, nullptr))
        << strerror(errno);
    EXPECT_GT(shared_group_id(base), 0);

    best_effort_umount(base);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, GroupIdIsReusedOnlyAfterLastPeerLeaves) {
    char base[160] = {};
    char slave[160] = {};
    char master[160] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(slave, sizeof(slave), "%s/slave", root_);
    snprintf(master, sizeof(master), "%s/master", root_);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(slave)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(master)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    const int original_group = shared_group_id(base);
    ASSERT_GT(original_group, 0);

    ASSERT_EQ(0, mount(base, slave, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(original_group, shared_group_id(slave));
    ASSERT_EQ(0, umount(base)) << strerror(errno);

    ASSERT_EQ(0, mount("", master, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, master, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    const int live_peer_group = shared_group_id(master);
    ASSERT_GT(live_peer_group, 0);
    EXPECT_NE(original_group, live_peer_group);

    ASSERT_EQ(0, umount(slave)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    EXPECT_EQ(original_group, shared_group_id(base));

    best_effort_umount(base);
    best_effort_umount(master);
    best_effort_rmdir(base);
    best_effort_rmdir(slave);
    best_effort_rmdir(master);
}

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

TEST_F(MountPropagationTest, BindSharedSourceIntoSharedTargetUpdatesPropagatedClone) {
    char src[160] = {};
    char target_a[160] = {};
    char target_b[160] = {};
    char bind_a[192] = {};
    char bind_b[192] = {};
    char src_host[192] = {};
    char bind_a_host[224] = {};
    char bind_b_host[224] = {};

    snprintf(src, sizeof(src), "%s/src", root_);
    snprintf(target_a, sizeof(target_a), "%s/target_a", root_);
    snprintf(target_b, sizeof(target_b), "%s/target_b", root_);
    snprintf(bind_a, sizeof(bind_a), "%s/bind", target_a);
    snprintf(bind_b, sizeof(bind_b), "%s/bind", target_b);
    snprintf(src_host, sizeof(src_host), "%s/host", src);
    snprintf(bind_a_host, sizeof(bind_a_host), "%s/host", bind_a);
    snprintf(bind_b_host, sizeof(bind_b_host), "%s/host", bind_b);

    ASSERT_EQ(0, ensure_dir(src)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(target_a)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(target_b)) << strerror(errno);
    ASSERT_EQ(0, mount("", src, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount("", target_a, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, src, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, target_a, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(target_a, target_b, nullptr, MS_BIND, nullptr)) << strerror(errno);

    ASSERT_EQ(0, ensure_dir(bind_a)) << strerror(errno);
    ASSERT_EQ(0, mount(src, bind_a, nullptr, MS_BIND, nullptr)) << strerror(errno);

    ASSERT_EQ(0, ensure_dir(src_host)) << strerror(errno);
    ASSERT_EQ(0, mount("", src_host, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(src_host, "host_marker")) << strerror(errno);
    EXPECT_TRUE(marker_exists(bind_a_host, "host_marker"));
    EXPECT_TRUE(marker_exists(bind_b_host, "host_marker"));

    best_effort_umount(src_host);
    best_effort_rmdir(src_host);
    best_effort_umount(bind_a);
    best_effort_rmdir(bind_a);
    best_effort_umount(target_b);
    best_effort_rmdir(target_b);
    best_effort_umount(target_a);
    best_effort_rmdir(target_a);
    best_effort_umount(src);
    best_effort_rmdir(src);
}

TEST_F(MountPropagationTest, BindSharedSlaveIntoSharedTargetRegistersPropagatedCloneAsSlave) {
    char master[160] = {};
    char source[160] = {};
    char target_a[160] = {};
    char target_b[160] = {};
    char bind_a[192] = {};
    char bind_b[192] = {};
    char master_host[192] = {};
    char source_host[192] = {};
    char bind_a_host[224] = {};
    char bind_b_host[224] = {};

    snprintf(master, sizeof(master), "%s/master", root_);
    snprintf(source, sizeof(source), "%s/source", root_);
    snprintf(target_a, sizeof(target_a), "%s/target_a", root_);
    snprintf(target_b, sizeof(target_b), "%s/target_b", root_);
    snprintf(bind_a, sizeof(bind_a), "%s/bind", target_a);
    snprintf(bind_b, sizeof(bind_b), "%s/bind", target_b);
    snprintf(master_host, sizeof(master_host), "%s/host", master);
    snprintf(source_host, sizeof(source_host), "%s/host", source);
    snprintf(bind_a_host, sizeof(bind_a_host), "%s/host", bind_a);
    snprintf(bind_b_host, sizeof(bind_b_host), "%s/host", bind_b);

    ASSERT_EQ(0, ensure_dir(master)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(source)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(target_a)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(target_b)) << strerror(errno);
    ASSERT_EQ(0, mount("", master, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount("", target_a, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, master, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(master, source, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, source, nullptr, MS_SLAVE, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, source, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, target_a, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(target_a, target_b, nullptr, MS_BIND, nullptr)) << strerror(errno);

    ASSERT_EQ(0, ensure_dir(bind_a)) << strerror(errno);
    ASSERT_EQ(0, mount(source, bind_a, nullptr, MS_BIND, nullptr)) << strerror(errno);

    ASSERT_EQ(0, ensure_dir(master_host)) << strerror(errno);
    ASSERT_EQ(0, mount("", master_host, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(master_host, "host_marker")) << strerror(errno);
    EXPECT_TRUE(marker_exists(source_host, "host_marker"));
    EXPECT_TRUE(marker_exists(bind_a_host, "host_marker"));
    EXPECT_TRUE(marker_exists(bind_b_host, "host_marker"));

    best_effort_umount(master_host);
    best_effort_rmdir(master_host);
    best_effort_umount(bind_a);
    best_effort_rmdir(bind_a);
    best_effort_umount(target_b);
    best_effort_rmdir(target_b);
    best_effort_umount(target_a);
    best_effort_rmdir(target_a);
    best_effort_umount(source);
    best_effort_rmdir(source);
    best_effort_umount(master);
    best_effort_rmdir(master);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
