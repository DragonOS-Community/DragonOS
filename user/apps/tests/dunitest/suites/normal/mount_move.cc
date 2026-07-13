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
#ifndef MS_MOVE
#define MS_MOVE 8192
#endif
#ifndef MS_BIND
#define MS_BIND 4096
#endif
#ifndef MS_SHARED
#define MS_SHARED (1 << 20)
#endif
#ifndef MS_PRIVATE
#define MS_PRIVATE (1 << 18)
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

// Read /proc/self/mountinfo and count how many times a path appears as a mount point.
int mountinfo_count_mountpoint(const char* mount_point) {
    FILE* fp = fopen("/proc/self/mountinfo", "r");
    if (fp == nullptr) {
        return -1;
    }
    char line[1024] = {};
    int count = 0;
    while (fgets(line, sizeof(line), fp) != nullptr) {
        // mountinfo format: mnt_id parent_id major:minor root mount_point ...
        // The 5th field (index 4) is the mount point path.
        char* saveptr = nullptr;
        int field = 0;
        char* tok = strtok_r(line, " ", &saveptr);
        while (tok != nullptr) {
            if (field == 4) {
                if (strcmp(tok, mount_point) == 0) {
                    count++;
                }
                break;
            }
            tok = strtok_r(nullptr, " ", &saveptr);
            field++;
        }
    }
    fclose(fp);
    return count;
}

bool mountinfo_has_mountpoint(const char* mount_point) {
    return mountinfo_count_mountpoint(mount_point) > 0;
}

void best_effort_umount(const char* path) {
    if (umount(path) != 0 && errno != EINVAL && errno != ENOENT) {
        ADD_FAILURE() << "umount failed for " << path << ": errno=" << errno << " ("
                      << strerror(errno) << ")";
    }
}

void best_effort_umount_all(const char* path) {
    for (int i = 0; i < 16; ++i) {
        if (umount(path) == 0) {
            continue;
        }
        if (errno == EINVAL || errno == ENOENT) {
            return;
        }
        ADD_FAILURE() << "umount failed for " << path << ": errno=" << errno << " ("
                      << strerror(errno) << ")";
        return;
    }
    ADD_FAILURE() << "too many stacked mounts while cleaning " << path;
}

void best_effort_rmdir(const char* path) {
    if (rmdir(path) != 0 && errno != ENOENT && errno != ENOTEMPTY) {
        ADD_FAILURE() << "rmdir failed for " << path << ": errno=" << errno << " ("
                      << strerror(errno) << ")";
    }
}

void cleanup_path(const char* path) {
    best_effort_umount_all(path);
    rmdir(path);
}

class MountMoveTest : public ::testing::Test {
protected:
    char root_[128] = {};

    void SetUp() override {
        ASSERT_EQ(0, ensure_dir("/tmp")) << strerror(errno);
        snprintf(root_, sizeof(root_), "/tmp/mount_move_%d", getpid());
        ASSERT_EQ(0, ensure_dir(root_)) << strerror(errno);

        if (unshare(CLONE_NEWNS) != 0) {
            GTEST_SKIP() << "unshare(CLONE_NEWNS): " << strerror(errno);
        }
        ASSERT_EQ(0, mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr))
            << strerror(errno);
    }

    void TearDown() override {
        char path[224] = {};
        // Clean up from deepest to shallowest where possible.
        const char* suffixes[] = {
            "/dst/child", "/src/child", "/dst_a/mp", "/dst_b/mp", "/dst_a", "/dst_b",
            "/base/sub",  "/base",      "/dst",      "/src",      "/dst_file",
        };
        for (const char* suffix : suffixes) {
            snprintf(path, sizeof(path), "%s%s", root_, suffix);
            cleanup_path(path);
        }
        for (int i = 0; i < 16; ++i) {
            snprintf(path, sizeof(path), "%s/src_%d", root_, i);
            cleanup_path(path);
            snprintf(path, sizeof(path), "%s/dst_%d", root_, i);
            cleanup_path(path);
        }
        // dst_file is a regular file, not a directory.
        snprintf(path, sizeof(path), "%s/dst_file", root_);
        unlink(path);
        best_effort_rmdir(root_);
    }
};

}  // namespace

// Basic move: relocate a mount from src to dst, verify content migrates and src is no longer a mount point.
TEST_F(MountMoveTest, BasicMoveRelocatesMount) {
    char src[160] = {};
    char dst[160] = {};
    snprintf(src, sizeof(src), "%s/src", root_);
    snprintf(dst, sizeof(dst), "%s/dst", root_);

    ASSERT_EQ(0, ensure_dir(src)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(dst)) << strerror(errno);
    ASSERT_EQ(0, mount("", src, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(src, "marker")) << strerror(errno);

    ASSERT_EQ(0, mount(src, dst, nullptr, MS_MOVE, nullptr)) << strerror(errno);

    EXPECT_TRUE(marker_exists(dst, "marker"));
    // src reverts to the underlying empty directory; the marker is no longer visible.
    EXPECT_FALSE(marker_exists(src, "marker"));
    EXPECT_TRUE(mountinfo_has_mountpoint(dst));
    EXPECT_FALSE(mountinfo_has_mountpoint(src));

    best_effort_umount(dst);
}

// Linux moves only the currently visible top mount at the source path. Lower stacked mounts
// at the same source path must stay behind and become visible again.
TEST_F(MountMoveTest, MoveOnlyTopOfStackKeepsLowerAtSource) {
    char src[160] = {};
    char dst[160] = {};
    snprintf(src, sizeof(src), "%s/src", root_);
    snprintf(dst, sizeof(dst), "%s/dst", root_);

    ASSERT_EQ(0, ensure_dir(src)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(dst)) << strerror(errno);
    ASSERT_EQ(0, mount("", src, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(src, "lower_marker")) << strerror(errno);
    ASSERT_EQ(0, mount("", src, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(src, "upper_marker")) << strerror(errno);

    ASSERT_EQ(0, mount(src, dst, nullptr, MS_MOVE, nullptr)) << strerror(errno);

    EXPECT_TRUE(marker_exists(dst, "upper_marker"));
    EXPECT_FALSE(marker_exists(dst, "lower_marker"));
    EXPECT_TRUE(marker_exists(src, "lower_marker"));
    EXPECT_FALSE(marker_exists(src, "upper_marker"));
    EXPECT_EQ(1, mountinfo_count_mountpoint(src));
    EXPECT_EQ(1, mountinfo_count_mountpoint(dst));

    best_effort_umount_all(dst);
    best_effort_umount_all(src);
}

// Moving onto an already-mounted target must place the moved mount above the old target mount.
TEST_F(MountMoveTest, MoveOntoMountedTargetBecomesTop) {
    for (int i = 0; i < 8; ++i) {
        char src[160] = {};
        char dst[160] = {};
        snprintf(src, sizeof(src), "%s/src_%d", root_, i);
        snprintf(dst, sizeof(dst), "%s/dst_%d", root_, i);

        ASSERT_EQ(0, ensure_dir(src)) << strerror(errno);
        ASSERT_EQ(0, ensure_dir(dst)) << strerror(errno);
        ASSERT_EQ(0, mount("", dst, "ramfs", 0, nullptr)) << strerror(errno);
        ASSERT_EQ(0, create_marker(dst, "target_marker")) << strerror(errno);
        ASSERT_EQ(0, mount("", src, "ramfs", 0, nullptr)) << strerror(errno);
        ASSERT_EQ(0, create_marker(src, "source_marker")) << strerror(errno);

        ASSERT_EQ(0, mount(src, dst, nullptr, MS_MOVE, nullptr)) << strerror(errno);

        EXPECT_TRUE(marker_exists(dst, "source_marker"));
        EXPECT_FALSE(marker_exists(dst, "target_marker"));
        // Linux exposes both the covered target mount and the moved top mount
        // as distinct mountinfo records at the same pathname.
        EXPECT_EQ(2, mountinfo_count_mountpoint(dst));

        best_effort_umount(dst);
        EXPECT_TRUE(marker_exists(dst, "target_marker"));
        EXPECT_FALSE(marker_exists(dst, "source_marker"));

        best_effort_umount_all(dst);
        best_effort_umount_all(src);
        best_effort_rmdir(dst);
        best_effort_rmdir(src);
    }
}

// Move a subtree with child mounts: child mounts migrate with the parent and mount_list path prefixes are updated.
TEST_F(MountMoveTest, MoveSubtreeUpdatesChildMountPath) {
    char src[160] = {};
    char dst[160] = {};
    char src_child[224] = {};
    char dst_child[224] = {};
    snprintf(src, sizeof(src), "%s/src", root_);
    snprintf(dst, sizeof(dst), "%s/dst", root_);
    snprintf(src_child, sizeof(src_child), "%s/child", src);
    snprintf(dst_child, sizeof(dst_child), "%s/child", dst);

    ASSERT_EQ(0, ensure_dir(src)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(dst)) << strerror(errno);
    ASSERT_EQ(0, mount("", src, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(src_child)) << strerror(errno);
    ASSERT_EQ(0, mount("", src_child, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(src_child, "child_marker")) << strerror(errno);

    ASSERT_EQ(0, mount(src, dst, nullptr, MS_MOVE, nullptr)) << strerror(errno);

    // Child mounts migrate with the parent mount to the new location.
    EXPECT_TRUE(marker_exists(dst_child, "child_marker"));
    EXPECT_TRUE(mountinfo_has_mountpoint(dst_child));
    EXPECT_FALSE(mountinfo_has_mountpoint(src_child));

    best_effort_umount(dst_child);
    best_effort_umount(dst);
}

// Moving a non-mountpoint directory should fail (path_mounted check).
TEST_F(MountMoveTest, MoveNonMountpointFails) {
    char src[160] = {};
    char dst[160] = {};
    snprintf(src, sizeof(src), "%s/src", root_);
    snprintf(dst, sizeof(dst), "%s/dst", root_);

    ASSERT_EQ(0, ensure_dir(src)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(dst)) << strerror(errno);

    // src is just a regular directory, not a mount point.
    EXPECT_NE(0, mount(src, dst, nullptr, MS_MOVE, nullptr));
    EXPECT_EQ(EINVAL, errno);
}

// Moving the namespace root should fail.
TEST_F(MountMoveTest, MoveNamespaceRootFails) {
    char dst[160] = {};
    snprintf(dst, sizeof(dst), "%s/dst", root_);
    ASSERT_EQ(0, ensure_dir(dst)) << strerror(errno);

    EXPECT_NE(0, mount("/", dst, nullptr, MS_MOVE, nullptr));
    EXPECT_EQ(EINVAL, errno);
}

// Source is a directory mount, target is a regular file; type mismatch should fail.
TEST_F(MountMoveTest, MoveTypeMismatchFails) {
    char src[160] = {};
    char dst_file[160] = {};
    snprintf(src, sizeof(src), "%s/src", root_);
    snprintf(dst_file, sizeof(dst_file), "%s/dst_file", root_);

    ASSERT_EQ(0, ensure_dir(src)) << strerror(errno);
    ASSERT_EQ(0, mount("", src, "ramfs", 0, nullptr)) << strerror(errno);

    int fd = open(dst_file, O_CREAT | O_WRONLY, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    close(fd);

    EXPECT_NE(0, mount(src, dst_file, nullptr, MS_MOVE, nullptr));
    EXPECT_EQ(ENOTDIR, errno);

    best_effort_umount(src);
}

// Cannot move a child mount from a shared parent mount (Linux: attached && IS_MNT_SHARED(parent)).
TEST_F(MountMoveTest, MoveFromSharedParentFails) {
    char base[160] = {};
    char sub[224] = {};
    char dst[160] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(sub, sizeof(sub), "%s/sub", base);
    snprintf(dst, sizeof(dst), "%s/dst", root_);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(dst)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(sub)) << strerror(errno);
    ASSERT_EQ(0, mount("", sub, "ramfs", 0, nullptr)) << strerror(errno);

    // sub's parent mount base is shared; moving sub should fail.
    EXPECT_NE(0, mount(sub, dst, nullptr, MS_MOVE, nullptr));
    EXPECT_EQ(EINVAL, errno);

    best_effort_umount(sub);
    best_effort_umount(base);
}

// Move into a shared target: the moved mount propagates to the target parent's peers.
TEST_F(MountMoveTest, MoveIntoSharedDestPropagatesToPeer) {
    char src[160] = {};
    char dst_a[160] = {};
    char dst_b[160] = {};
    char mp_a[224] = {};
    char mp_b[224] = {};
    snprintf(src, sizeof(src), "%s/src", root_);
    snprintf(dst_a, sizeof(dst_a), "%s/dst_a", root_);
    snprintf(dst_b, sizeof(dst_b), "%s/dst_b", root_);
    snprintf(mp_a, sizeof(mp_a), "%s/mp", dst_a);
    snprintf(mp_b, sizeof(mp_b), "%s/mp", dst_b);

    ASSERT_EQ(0, ensure_dir(src)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(dst_a)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(dst_b)) << strerror(errno);

    // Establish a shared peer pair: dst_a and dst_b are bind peers of the same ramfs.
    ASSERT_EQ(0, mount("", dst_a, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, dst_a, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(dst_a, dst_b, nullptr, MS_BIND, nullptr)) << strerror(errno);

    // Create a mount point directory on the shared underlying ramfs, visible to both peers.
    ASSERT_EQ(0, ensure_dir(mp_a)) << strerror(errno);

    // The private mount to be moved.
    ASSERT_EQ(0, mount("", src, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(src, "moved_marker")) << strerror(errno);

    ASSERT_EQ(0, mount(src, mp_a, nullptr, MS_MOVE, nullptr)) << strerror(errno);

    // After moving to the shared target, clone propagates to peer dst_b/mp.
    EXPECT_TRUE(marker_exists(mp_a, "moved_marker"));
    EXPECT_TRUE(marker_exists(mp_b, "moved_marker"));

    best_effort_umount(mp_b);
    best_effort_umount(mp_a);
    best_effort_umount(dst_b);
    best_effort_umount(dst_a);
}

// After move, unshare(CLONE_NEWNS) again: verify that userspace MS_MOVE updates the mount_list
// root record inode, so copy_mnt_ns() traversing mountpoints to look up paths no longer mismatches
// (regression test for a previous panic).
TEST_F(MountMoveTest, MoveThenUnshareKeepsCopyConsistent) {
    char src[160] = {};
    char dst[160] = {};
    snprintf(src, sizeof(src), "%s/src", root_);
    snprintf(dst, sizeof(dst), "%s/dst", root_);

    ASSERT_EQ(0, ensure_dir(src)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(dst)) << strerror(errno);
    ASSERT_EQ(0, mount("", src, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(src, "moved_marker")) << strerror(errno);

    // Userspace MS_MOVE: src's mount point inode changes from src to dst.
    ASSERT_EQ(0, mount(src, dst, nullptr, MS_MOVE, nullptr)) << strerror(errno);
    ASSERT_TRUE(mountinfo_has_mountpoint(dst));

    // Critical regression point: clone mount namespace after move. copy_mnt_ns() traverses
    // mountpoints and uses the mount point inode to look up mount_list paths. If the move
    // did not sync the root record's inode, this would trigger a kernel panic / failure.
    ASSERT_EQ(0, unshare(CLONE_NEWNS)) << strerror(errno);

    // The new namespace should fully inherit the moved mount and its contents.
    EXPECT_TRUE(mountinfo_has_mountpoint(dst));
    EXPECT_TRUE(marker_exists(dst, "moved_marker"));
    EXPECT_FALSE(mountinfo_has_mountpoint(src));

    best_effort_umount(dst);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
