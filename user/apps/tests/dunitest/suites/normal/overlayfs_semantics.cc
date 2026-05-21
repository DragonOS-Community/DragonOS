#include <gtest/gtest.h>

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <unistd.h>

namespace {

int ensure_dir(const char* path) {
    struct stat st = {};
    if (stat(path, &st) == 0) {
        return S_ISDIR(st.st_mode) ? 0 : -1;
    }
    return mkdir(path, 0755);
}

void remove_tree(const char* root) {
    char path[256] = {};

    snprintf(path, sizeof(path), "%s/m", root);
    umount(path);
    rmdir(path);
    snprintf(path, sizeof(path), "%s/u/x", root);
    unlink(path);
    snprintf(path, sizeof(path), "%s/u/x", root);
    rmdir(path);
    snprintf(path, sizeof(path), "%s/l/x", root);
    unlink(path);
    snprintf(path, sizeof(path), "%s/u", root);
    rmdir(path);
    snprintf(path, sizeof(path), "%s/l", root);
    rmdir(path);
    snprintf(path, sizeof(path), "%s/w", root);
    rmdir(path);
    rmdir(root);
}

void alarm_handler(int) {
    _exit(124);
}

}  // namespace

TEST(OverlayFsSemantics, ListAndLookupUpperDirOverLowerFile) {
    char root[128] = {};
    char upper[160] = {};
    char lower[160] = {};
    char work[160] = {};
    char merged[160] = {};
    char upper_x[192] = {};
    char lower_x[192] = {};
    char merged_x[192] = {};
    char options[512] = {};

    snprintf(root, sizeof(root), "/tmp/overlayfs_semantics_%d", getpid());
    snprintf(upper, sizeof(upper), "%s/u", root);
    snprintf(lower, sizeof(lower), "%s/l", root);
    snprintf(work, sizeof(work), "%s/w", root);
    snprintf(merged, sizeof(merged), "%s/m", root);
    snprintf(upper_x, sizeof(upper_x), "%s/x", upper);
    snprintf(lower_x, sizeof(lower_x), "%s/x", lower);
    snprintf(merged_x, sizeof(merged_x), "%s/x", merged);

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(root));
    ASSERT_EQ(0, ensure_dir(upper));
    ASSERT_EQ(0, ensure_dir(lower));
    ASSERT_EQ(0, ensure_dir(work));
    ASSERT_EQ(0, ensure_dir(merged));
    ASSERT_EQ(0, mkdir(upper_x, 0755));

    FILE* lower_file = fopen(lower_x, "w");
    ASSERT_NE(nullptr, lower_file) << strerror(errno);
    fclose(lower_file);

    snprintf(options, sizeof(options), "lowerdir=%s,upperdir=%s,workdir=%s", lower, upper, work);
    if (mount("overlay", merged, "overlay", 0, options) != 0) {
        remove_tree(root);
        GTEST_SKIP() << strerror(errno);
    }

    signal(SIGALRM, alarm_handler);
    alarm(5);

    DIR* dir = opendir(merged);
    if (dir != nullptr) {
        while (readdir(dir) != nullptr) {
        }
        closedir(dir);
    } else if (errno != ENOSYS) {
        FAIL() << strerror(errno);
    }

    struct stat st = {};
    ASSERT_EQ(0, stat(merged_x, &st)) << strerror(errno);
    EXPECT_TRUE(S_ISDIR(st.st_mode));

    alarm(0);
    remove_tree(root);
}

TEST(OverlayFsSemantics, CreateOverWhiteoutAfterLowerUnlink) {
    char root[128] = {};
    char upper[160] = {};
    char lower[160] = {};
    char work[160] = {};
    char merged[160] = {};
    char lower_x[192] = {};
    char merged_x[192] = {};
    char options[512] = {};

    snprintf(root, sizeof(root), "/tmp/overlayfs_whiteout_%d", getpid());
    snprintf(upper, sizeof(upper), "%s/u", root);
    snprintf(lower, sizeof(lower), "%s/l", root);
    snprintf(work, sizeof(work), "%s/w", root);
    snprintf(merged, sizeof(merged), "%s/m", root);
    snprintf(lower_x, sizeof(lower_x), "%s/x", lower);
    snprintf(merged_x, sizeof(merged_x), "%s/x", merged);

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(root));
    ASSERT_EQ(0, ensure_dir(upper));
    ASSERT_EQ(0, ensure_dir(lower));
    ASSERT_EQ(0, ensure_dir(work));
    ASSERT_EQ(0, ensure_dir(merged));

    FILE* lower_file = fopen(lower_x, "w");
    ASSERT_NE(nullptr, lower_file) << strerror(errno);
    ASSERT_EQ(5U, fwrite("lower", 1, 5, lower_file));
    fclose(lower_file);

    snprintf(options, sizeof(options), "lowerdir=%s,upperdir=%s,workdir=%s", lower, upper, work);
    if (mount("overlay", merged, "overlay", 0, options) != 0) {
        remove_tree(root);
        GTEST_SKIP() << strerror(errno);
    }

    ASSERT_EQ(0, unlink(merged_x)) << strerror(errno);

    struct stat st = {};
    ASSERT_EQ(-1, stat(merged_x, &st));
    ASSERT_EQ(ENOENT, errno);

    int fd = open(merged_x, O_CREAT | O_WRONLY | O_EXCL, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(10, write(fd, "upper-data", 10)) << strerror(errno);
    close(fd);

    ASSERT_EQ(0, stat(merged_x, &st)) << strerror(errno);
    EXPECT_TRUE(S_ISREG(st.st_mode));
    EXPECT_EQ(10, st.st_size);

    remove_tree(root);
}

TEST(OverlayFsSemantics, LowerWhiteoutHidesLowerLayers) {
    char root[128] = {};
    char upper[160] = {};
    char lower_top[160] = {};
    char lower_bottom[160] = {};
    char work[160] = {};
    char merged[160] = {};
    char top_x[192] = {};
    char bottom_x[192] = {};
    char merged_x[192] = {};
    char options[512] = {};

    snprintf(root, sizeof(root), "/tmp/overlayfs_lower_whiteout_%d", getpid());
    snprintf(upper, sizeof(upper), "%s/u", root);
    snprintf(lower_top, sizeof(lower_top), "%s/l1", root);
    snprintf(lower_bottom, sizeof(lower_bottom), "%s/l2", root);
    snprintf(work, sizeof(work), "%s/w", root);
    snprintf(merged, sizeof(merged), "%s/m", root);
    snprintf(top_x, sizeof(top_x), "%s/x", lower_top);
    snprintf(bottom_x, sizeof(bottom_x), "%s/x", lower_bottom);
    snprintf(merged_x, sizeof(merged_x), "%s/x", merged);

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(root));
    ASSERT_EQ(0, ensure_dir(upper));
    ASSERT_EQ(0, ensure_dir(lower_top));
    ASSERT_EQ(0, ensure_dir(lower_bottom));
    ASSERT_EQ(0, ensure_dir(work));
    ASSERT_EQ(0, ensure_dir(merged));
    ASSERT_EQ(0, mknod(top_x, S_IFCHR | 0600, makedev(0, 0))) << strerror(errno);

    FILE* bottom_file = fopen(bottom_x, "w");
    ASSERT_NE(nullptr, bottom_file) << strerror(errno);
    ASSERT_EQ(5U, fwrite("lower", 1, 5, bottom_file));
    fclose(bottom_file);

    snprintf(options, sizeof(options), "lowerdir=%s:%s,upperdir=%s,workdir=%s",
             lower_top, lower_bottom, upper, work);
    if (mount("overlay", merged, "overlay", 0, options) != 0) {
        remove_tree(root);
        GTEST_SKIP() << strerror(errno);
    }

    struct stat st = {};
    ASSERT_EQ(-1, stat(merged_x, &st));
    EXPECT_EQ(ENOENT, errno);

    remove_tree(root);
    unlink(top_x);
    unlink(bottom_x);
    rmdir(lower_top);
    rmdir(lower_bottom);
    rmdir(root);
}

TEST(OverlayFsSemantics, RmdirLowerOnlyNonEmptyDirFails) {
    char root[128] = {};
    char upper[160] = {};
    char lower[160] = {};
    char work[160] = {};
    char merged[160] = {};
    char lower_dir[192] = {};
    char lower_child[224] = {};
    char merged_dir[192] = {};
    char options[512] = {};

    snprintf(root, sizeof(root), "/tmp/overlayfs_rmdir_lower_%d", getpid());
    snprintf(upper, sizeof(upper), "%s/u", root);
    snprintf(lower, sizeof(lower), "%s/l", root);
    snprintf(work, sizeof(work), "%s/w", root);
    snprintf(merged, sizeof(merged), "%s/m", root);
    snprintf(lower_dir, sizeof(lower_dir), "%s/sub", lower);
    snprintf(lower_child, sizeof(lower_child), "%s/child", lower_dir);
    snprintf(merged_dir, sizeof(merged_dir), "%s/sub", merged);

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(root));
    ASSERT_EQ(0, ensure_dir(upper));
    ASSERT_EQ(0, ensure_dir(lower));
    ASSERT_EQ(0, ensure_dir(work));
    ASSERT_EQ(0, ensure_dir(merged));
    ASSERT_EQ(0, mkdir(lower_dir, 0755));

    FILE* child_file = fopen(lower_child, "w");
    ASSERT_NE(nullptr, child_file) << strerror(errno);
    ASSERT_EQ(5U, fwrite("child", 1, 5, child_file));
    fclose(child_file);

    snprintf(options, sizeof(options), "lowerdir=%s,upperdir=%s,workdir=%s", lower, upper, work);
    if (mount("overlay", merged, "overlay", 0, options) != 0) {
        remove_tree(root);
        GTEST_SKIP() << strerror(errno);
    }

    ASSERT_EQ(-1, rmdir(merged_dir));
    EXPECT_EQ(ENOTEMPTY, errno);

    struct stat st = {};
    ASSERT_EQ(0, stat(merged_dir, &st)) << strerror(errno);
    EXPECT_TRUE(S_ISDIR(st.st_mode));

    remove_tree(root);
    unlink(lower_child);
    rmdir(lower_dir);
    rmdir(lower);
    rmdir(root);
}

TEST(OverlayFsSemantics, UnlinkLowerWhiteoutReturnsEnoent) {
    char root[128] = {};
    char upper[160] = {};
    char lower_top[160] = {};
    char lower_bottom[160] = {};
    char work[160] = {};
    char merged[160] = {};
    char top_x[192] = {};
    char bottom_x[192] = {};
    char merged_x[192] = {};
    char options[512] = {};

    snprintf(root, sizeof(root), "/tmp/overlayfs_unlink_whiteout_%d", getpid());
    snprintf(upper, sizeof(upper), "%s/u", root);
    snprintf(lower_top, sizeof(lower_top), "%s/l1", root);
    snprintf(lower_bottom, sizeof(lower_bottom), "%s/l2", root);
    snprintf(work, sizeof(work), "%s/w", root);
    snprintf(merged, sizeof(merged), "%s/m", root);
    snprintf(top_x, sizeof(top_x), "%s/x", lower_top);
    snprintf(bottom_x, sizeof(bottom_x), "%s/x", lower_bottom);
    snprintf(merged_x, sizeof(merged_x), "%s/x", merged);

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(root));
    ASSERT_EQ(0, ensure_dir(upper));
    ASSERT_EQ(0, ensure_dir(lower_top));
    ASSERT_EQ(0, ensure_dir(lower_bottom));
    ASSERT_EQ(0, ensure_dir(work));
    ASSERT_EQ(0, ensure_dir(merged));
    ASSERT_EQ(0, mknod(top_x, S_IFCHR | 0600, makedev(0, 0))) << strerror(errno);

    FILE* bottom_file = fopen(bottom_x, "w");
    ASSERT_NE(nullptr, bottom_file) << strerror(errno);
    ASSERT_EQ(5U, fwrite("lower", 1, 5, bottom_file));
    fclose(bottom_file);

    snprintf(options, sizeof(options), "lowerdir=%s:%s,upperdir=%s,workdir=%s",
             lower_top, lower_bottom, upper, work);
    if (mount("overlay", merged, "overlay", 0, options) != 0) {
        remove_tree(root);
        unlink(top_x);
        unlink(bottom_x);
        rmdir(lower_top);
        rmdir(lower_bottom);
        rmdir(root);
        GTEST_SKIP() << strerror(errno);
    }

    ASSERT_EQ(-1, unlink(merged_x));
    EXPECT_EQ(ENOENT, errno);

    struct stat st = {};
    ASSERT_EQ(-1, stat(merged_x, &st));
    EXPECT_EQ(ENOENT, errno);

    remove_tree(root);
    unlink(top_x);
    unlink(bottom_x);
    rmdir(lower_top);
    rmdir(lower_bottom);
    rmdir(root);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
