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
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <algorithm>
#include <string>
#include <unistd.h>

namespace {

#ifndef __NR_renameat2
#define __NR_renameat2 316
#endif

#ifndef RENAME_NOREPLACE
#define RENAME_NOREPLACE (1U << 0)
#endif

#ifndef RENAME_EXCHANGE
#define RENAME_EXCHANGE (1U << 1)
#endif

#ifndef RENAME_WHITEOUT
#define RENAME_WHITEOUT (1U << 2)
#endif

int ensure_dir(const char* path) {
    struct stat st = {};
    if (stat(path, &st) == 0) {
        return S_ISDIR(st.st_mode) ? 0 : -1;
    }
    return mkdir(path, 0755);
}

std::string join_path(const std::string& dir, const char* name) {
    return dir + "/" + name;
}

bool path_exists(const std::string& path) {
    struct stat st = {};
    return stat(path.c_str(), &st) == 0;
}

bool is_whiteout(const std::string& path) {
    struct stat st = {};
    if (lstat(path.c_str(), &st) != 0) {
        return false;
    }
    return S_ISCHR(st.st_mode) && major(st.st_rdev) == 0 && minor(st.st_rdev) == 0;
}

int write_text(const std::string& path, const char* text) {
    int fd = open(path.c_str(), O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        return -1;
    }
    size_t len = strlen(text);
    ssize_t written = write(fd, text, len);
    int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return written == static_cast<ssize_t>(len) ? 0 : -1;
}

std::string read_text(const std::string& path) {
    char buf[128] = {};
    int fd = open(path.c_str(), O_RDONLY);
    if (fd < 0) {
        return {};
    }
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    if (n < 0) {
        return {};
    }
    return std::string(buf, static_cast<size_t>(n));
}

int overlay_temp_entry_count(const std::string& dir_path) {
    DIR* dir = opendir(dir_path.c_str());
    if (dir == nullptr) {
        return -1;
    }

    int count = 0;
    while (dirent* ent = readdir(dir)) {
        if (strncmp(ent->d_name, ".dragonos-ovl-", strlen(".dragonos-ovl-")) == 0) {
            count++;
        }
    }
    closedir(dir);
    return count;
}

bool write_pattern_file(const std::string& path, size_t size) {
    int fd = open(path.c_str(), O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        return false;
    }

    char buf[4096] = {};
    size_t offset = 0;
    while (offset < size) {
        size_t chunk = std::min(sizeof(buf), size - offset);
        for (size_t i = 0; i < chunk; ++i) {
            buf[i] = static_cast<char>((offset + i) & 0xff);
        }

        ssize_t written = write(fd, buf, chunk);
        if (written != static_cast<ssize_t>(chunk)) {
            int saved_errno = errno;
            close(fd);
            errno = saved_errno;
            return false;
        }
        offset += chunk;
    }

    return close(fd) == 0;
}

bool validate_pattern_window(int fd, size_t offset, size_t len) {
    char buf[4096] = {};
    if (len > sizeof(buf)) {
        return false;
    }

    ssize_t n = pread(fd, buf, len, static_cast<off_t>(offset));
    if (n != static_cast<ssize_t>(len)) {
        return false;
    }

    for (size_t i = 0; i < len; ++i) {
        if (buf[i] != static_cast<char>((offset + i) & 0xff)) {
            return false;
        }
    }
    return true;
}

bool validate_pattern_file(const std::string& path, size_t size) {
    int fd = open(path.c_str(), O_RDONLY);
    if (fd < 0) {
        return false;
    }

    bool ok = validate_pattern_window(fd, 0, 4096)
        && validate_pattern_window(fd, size / 2, 4096)
        && validate_pattern_window(fd, size - 4096, 4096);
    int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return ok;
}

long renameat2_call(const std::string& old_path, const std::string& new_path, unsigned flags) {
    return syscall(__NR_renameat2, AT_FDCWD, old_path.c_str(), AT_FDCWD, new_path.c_str(), flags);
}

void remove_recursive(const std::string& path) {
    struct stat st = {};
    if (lstat(path.c_str(), &st) != 0) {
        return;
    }
    if (!S_ISDIR(st.st_mode)) {
        unlink(path.c_str());
        return;
    }

    DIR* dir = opendir(path.c_str());
    if (dir != nullptr) {
        while (dirent* ent = readdir(dir)) {
            if (strcmp(ent->d_name, ".") == 0 || strcmp(ent->d_name, "..") == 0) {
                continue;
            }
            remove_recursive(join_path(path, ent->d_name));
        }
        closedir(dir);
    }
    rmdir(path.c_str());
}

struct OverlayRenameEnv {
    std::string root;
    std::string upper;
    std::string lower;
    std::string work;
    std::string merged;
};

OverlayRenameEnv make_overlay_env(const char* name) {
    std::string root = std::string("/tmp/") + name + "_" + std::to_string(getpid());
    OverlayRenameEnv env = {};
    env.root = root;
    env.upper = join_path(root, "u");
    env.lower = join_path(root, "l");
    env.work = join_path(root, "w");
    env.merged = join_path(root, "m");
    return env;
}

void cleanup_overlay_env(const OverlayRenameEnv& env) {
    umount(env.merged.c_str());
    remove_recursive(env.root);
}

struct ScopedOverlayEnv {
    explicit ScopedOverlayEnv(const char* name) : env(make_overlay_env(name)) {}

    ~ScopedOverlayEnv() {
        cleanup_overlay_env(env);
    }

    OverlayRenameEnv env;
};

bool setup_overlay_env(const OverlayRenameEnv& env) {
    if (ensure_dir("/tmp") != 0 || ensure_dir(env.root.c_str()) != 0
        || ensure_dir(env.upper.c_str()) != 0 || ensure_dir(env.lower.c_str()) != 0
        || ensure_dir(env.work.c_str()) != 0 || ensure_dir(env.merged.c_str()) != 0) {
        cleanup_overlay_env(env);
        return false;
    }
    std::string options =
        "lowerdir=" + env.lower + ",upperdir=" + env.upper + ",workdir=" + env.work;
    if (mount("overlay", env.merged.c_str(), "overlay", 0, options.c_str()) != 0) {
        cleanup_overlay_env(env);
        return false;
    }
    return true;
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

TEST(OverlayFsSemantics, MkdirOverWhiteoutAfterLowerUnlink) {
    char root[128] = {};
    char upper[160] = {};
    char lower[160] = {};
    char work[160] = {};
    char merged[160] = {};
    char lower_x[192] = {};
    char merged_x[192] = {};
    char options[512] = {};

    snprintf(root, sizeof(root), "/tmp/overlayfs_whiteout_mkdir_%d", getpid());
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
    fclose(lower_file);

    snprintf(options, sizeof(options), "lowerdir=%s,upperdir=%s,workdir=%s", lower, upper, work);
    if (mount("overlay", merged, "overlay", 0, options) != 0) {
        remove_tree(root);
        GTEST_SKIP() << strerror(errno);
    }

    ASSERT_EQ(0, unlink(merged_x)) << strerror(errno);
    ASSERT_EQ(0, mkdir(merged_x, 0755)) << strerror(errno);

    struct stat st = {};
    ASSERT_EQ(0, stat(merged_x, &st)) << strerror(errno);
    EXPECT_TRUE(S_ISDIR(st.st_mode));

    remove_tree(root);
}

TEST(OverlayFsSemantics, MknodWhiteoutOnOverlayIsDenied) {
    char root[128] = {};
    char upper[160] = {};
    char lower[160] = {};
    char work[160] = {};
    char merged[160] = {};
    char lower_x[192] = {};
    char merged_x[192] = {};
    char options[512] = {};

    snprintf(root, sizeof(root), "/tmp/overlayfs_whiteout_mknod_%d", getpid());
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
    fclose(lower_file);

    snprintf(options, sizeof(options), "lowerdir=%s,upperdir=%s,workdir=%s", lower, upper, work);
    if (mount("overlay", merged, "overlay", 0, options) != 0) {
        remove_tree(root);
        GTEST_SKIP() << strerror(errno);
    }

    ASSERT_EQ(0, unlink(merged_x)) << strerror(errno);
    ASSERT_EQ(-1, mknod(merged_x, S_IFCHR | 0600, makedev(0, 0)));
    EXPECT_EQ(EPERM, errno);

    struct stat st = {};
    ASSERT_EQ(-1, stat(merged_x, &st));
    EXPECT_EQ(ENOENT, errno);

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

TEST(OverlayFsSemantics, MkdirUnderLowerOnlyDirCopiesUpParent) {
    char root[128] = {};
    char upper[160] = {};
    char lower[160] = {};
    char work[160] = {};
    char merged[160] = {};
    char lower_dev[192] = {};
    char lower_pts[224] = {};
    char upper_dev[192] = {};
    char upper_pts[224] = {};
    char merged_pts[224] = {};
    char options[512] = {};

    snprintf(root, sizeof(root), "/tmp/overlayfs_mkdir_lower_%d", getpid());
    snprintf(upper, sizeof(upper), "%s/u", root);
    snprintf(lower, sizeof(lower), "%s/l", root);
    snprintf(work, sizeof(work), "%s/w", root);
    snprintf(merged, sizeof(merged), "%s/m", root);
    snprintf(lower_dev, sizeof(lower_dev), "%s/dev", lower);
    snprintf(lower_pts, sizeof(lower_pts), "%s/pts", lower_dev);
    snprintf(upper_dev, sizeof(upper_dev), "%s/dev", upper);
    snprintf(upper_pts, sizeof(upper_pts), "%s/pts", upper_dev);
    snprintf(merged_pts, sizeof(merged_pts), "%s/dev/pts", merged);

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(root));
    ASSERT_EQ(0, ensure_dir(upper));
    ASSERT_EQ(0, ensure_dir(lower));
    ASSERT_EQ(0, ensure_dir(work));
    ASSERT_EQ(0, ensure_dir(merged));
    ASSERT_EQ(0, mkdir(lower_dev, 0755));

    snprintf(options, sizeof(options), "lowerdir=%s,upperdir=%s,workdir=%s", lower, upper, work);
    if (mount("overlay", merged, "overlay", 0, options) != 0) {
        rmdir(merged);
        rmdir(work);
        rmdir(lower_dev);
        rmdir(lower);
        rmdir(upper);
        rmdir(root);
        GTEST_SKIP() << strerror(errno);
    }

    ASSERT_EQ(0, mkdir(merged_pts, 0755)) << strerror(errno);

    struct stat st = {};
    ASSERT_EQ(0, stat(merged_pts, &st)) << strerror(errno);
    EXPECT_TRUE(S_ISDIR(st.st_mode));
    ASSERT_EQ(0, stat(upper_dev, &st)) << strerror(errno);
    EXPECT_TRUE(S_ISDIR(st.st_mode));
    ASSERT_EQ(0, stat(upper_pts, &st)) << strerror(errno);
    EXPECT_TRUE(S_ISDIR(st.st_mode));
    ASSERT_EQ(-1, stat(lower_pts, &st));
    EXPECT_EQ(ENOENT, errno);

    umount(merged);
    rmdir(merged);
    rmdir(upper_pts);
    rmdir(upper_dev);
    rmdir(work);
    rmdir(lower_dev);
    rmdir(lower);
    rmdir(upper);
    rmdir(root);
}

TEST(OverlayFsSemantics, BindMountOnOverlayChildUsesNamespacePath) {
    char root[128] = {};
    char upper[160] = {};
    char lower[160] = {};
    char work[160] = {};
    char merged[160] = {};
    char lower_tmp[192] = {};
    char source[160] = {};
    char source_file[192] = {};
    char merged_tmp[192] = {};
    char mounted_file[224] = {};
    char options[512] = {};

    snprintf(root, sizeof(root), "/tmp/overlayfs_bind_child_%d", getpid());
    snprintf(upper, sizeof(upper), "%s/u", root);
    snprintf(lower, sizeof(lower), "%s/l", root);
    snprintf(work, sizeof(work), "%s/w", root);
    snprintf(merged, sizeof(merged), "%s/m", root);
    snprintf(lower_tmp, sizeof(lower_tmp), "%s/tmp", lower);
    snprintf(source, sizeof(source), "%s/src", root);
    snprintf(source_file, sizeof(source_file), "%s/token", source);
    snprintf(merged_tmp, sizeof(merged_tmp), "%s/tmp", merged);
    snprintf(mounted_file, sizeof(mounted_file), "%s/token", merged_tmp);

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(root));
    ASSERT_EQ(0, ensure_dir(upper));
    ASSERT_EQ(0, ensure_dir(lower));
    ASSERT_EQ(0, ensure_dir(work));
    ASSERT_EQ(0, ensure_dir(merged));
    ASSERT_EQ(0, ensure_dir(source));
    ASSERT_EQ(0, mkdir(lower_tmp, 0755));

    FILE* fp = fopen(source_file, "w");
    ASSERT_NE(nullptr, fp) << strerror(errno);
    ASSERT_EQ(5U, fwrite("token", 1, 5, fp));
    fclose(fp);

    snprintf(options, sizeof(options), "lowerdir=%s,upperdir=%s,workdir=%s", lower, upper, work);
    if (mount("overlay", merged, "overlay", 0, options) != 0) {
        unlink(source_file);
        rmdir(source);
        rmdir(merged);
        rmdir(work);
        rmdir(lower_tmp);
        rmdir(lower);
        rmdir(upper);
        rmdir(root);
        GTEST_SKIP() << strerror(errno);
    }

    ASSERT_EQ(0, mount(source, merged_tmp, nullptr, MS_BIND | MS_REC, nullptr)) << strerror(errno);

    struct stat st = {};
    ASSERT_EQ(0, stat(mounted_file, &st)) << strerror(errno);
    EXPECT_TRUE(S_ISREG(st.st_mode));

    umount(merged_tmp);
    umount(merged);
    unlink(source_file);
    rmdir(source);
    rmdir(merged);
    rmdir(work);
    rmdir(lower_tmp);
    rmdir(lower);
    rmdir(upper);
    rmdir(root);
}

TEST(OverlayFsSemantics, OpenOverlayDirectoryWithoutFsOpenHook) {
    char root[128] = {};
    char upper[160] = {};
    char lower[160] = {};
    char work[160] = {};
    char merged[160] = {};
    char lower_dir[192] = {};
    char merged_dir[192] = {};
    char options[512] = {};

    snprintf(root, sizeof(root), "/tmp/overlayfs_open_dir_%d", getpid());
    snprintf(upper, sizeof(upper), "%s/u", root);
    snprintf(lower, sizeof(lower), "%s/l", root);
    snprintf(work, sizeof(work), "%s/w", root);
    snprintf(merged, sizeof(merged), "%s/m", root);
    snprintf(lower_dir, sizeof(lower_dir), "%s/dir", lower);
    snprintf(merged_dir, sizeof(merged_dir), "%s/dir", merged);

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(root));
    ASSERT_EQ(0, ensure_dir(upper));
    ASSERT_EQ(0, ensure_dir(lower));
    ASSERT_EQ(0, ensure_dir(work));
    ASSERT_EQ(0, ensure_dir(merged));
    ASSERT_EQ(0, mkdir(lower_dir, 0755));

    snprintf(options, sizeof(options), "lowerdir=%s,upperdir=%s,workdir=%s", lower, upper, work);
    if (mount("overlay", merged, "overlay", 0, options) != 0) {
        rmdir(merged);
        rmdir(work);
        rmdir(lower_dir);
        rmdir(lower);
        rmdir(upper);
        rmdir(root);
        GTEST_SKIP() << strerror(errno);
    }

    int root_fd = open(merged, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    ASSERT_GE(root_fd, 0) << strerror(errno);
    close(root_fd);

    int child_fd = open(merged_dir, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    ASSERT_GE(child_fd, 0) << strerror(errno);
    close(child_fd);

    umount(merged);
    rmdir(merged);
    rmdir(work);
    rmdir(lower_dir);
    rmdir(lower);
    rmdir(upper);
    rmdir(root);
}

TEST(OverlayFsSemantics, CopyUpLowerFilePublishesCompleteUpperFile) {
    ScopedOverlayEnv scoped("overlayfs_copy_up_file");
    const auto& env = scoped.env;
    std::string lower_file = join_path(env.lower, "file");
    std::string upper_file = join_path(env.upper, "file");
    std::string merged_file = join_path(env.merged, "file");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(lower_file, "lower-copy-up-data"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    int fd = open(merged_file.c_str(), O_WRONLY | O_CLOEXEC);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);

    EXPECT_EQ("lower-copy-up-data", read_text(upper_file));
    EXPECT_EQ("lower-copy-up-data", read_text(merged_file));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, CopyUpNestedLowerFileUsesWorkdirTempAndKeepsLeafAtomic) {
    ScopedOverlayEnv scoped("overlayfs_copy_up_nested");
    const auto& env = scoped.env;
    std::string lower_a = join_path(env.lower, "a");
    std::string lower_b = join_path(lower_a, "b");
    std::string lower_file = join_path(lower_b, "file");
    std::string upper_a = join_path(env.upper, "a");
    std::string upper_b = join_path(upper_a, "b");
    std::string upper_file = join_path(upper_b, "file");
    std::string merged_file = join_path(join_path(join_path(env.merged, "a"), "b"), "file");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, mkdir(lower_a.c_str(), 0755));
    ASSERT_EQ(0, mkdir(lower_b.c_str(), 0755));
    ASSERT_EQ(0, write_text(lower_file, "nested-copy-up-data"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    int fd = open(merged_file.c_str(), O_WRONLY | O_CLOEXEC);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);

    struct stat st = {};
    ASSERT_EQ(0, stat(upper_a.c_str(), &st)) << strerror(errno);
    EXPECT_TRUE(S_ISDIR(st.st_mode));
    ASSERT_EQ(0, stat(upper_b.c_str(), &st)) << strerror(errno);
    EXPECT_TRUE(S_ISDIR(st.st_mode));
    EXPECT_EQ("nested-copy-up-data", read_text(upper_file));
    EXPECT_EQ("nested-copy-up-data", read_text(merged_file));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, CopyUpTruncatePublishesEmptyUpperFile) {
    ScopedOverlayEnv scoped("overlayfs_copy_up_truncate");
    const auto& env = scoped.env;
    std::string lower_file = join_path(env.lower, "file");
    std::string upper_file = join_path(env.upper, "file");
    std::string merged_file = join_path(env.merged, "file");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(lower_file, "must-not-be-published-before-truncate"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    int fd = open(merged_file.c_str(), O_WRONLY | O_TRUNC | O_CLOEXEC);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);

    struct stat st = {};
    ASSERT_EQ(0, stat(upper_file.c_str(), &st)) << strerror(errno);
    EXPECT_EQ(0, st.st_size);
    ASSERT_EQ(0, stat(merged_file.c_str(), &st)) << strerror(errno);
    EXPECT_EQ(0, st.st_size);
    EXPECT_EQ("", read_text(upper_file));
    EXPECT_EQ("", read_text(merged_file));
    EXPECT_EQ("must-not-be-published-before-truncate", read_text(lower_file));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, CopyUpTruncateDropsPrivilegedBitsWithoutCapFsetid) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root to prepare setuid/setgid lower file";
    }

    ScopedOverlayEnv scoped("overlayfs_copy_up_truncate_mode");
    const auto& env = scoped.env;
    std::string lower_file = join_path(env.lower, "file");
    std::string upper_file = join_path(env.upper, "file");
    std::string merged_file = join_path(env.merged, "file");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(lower_file, "truncate should clear privileged bits"));
    ASSERT_EQ(0, chmod(lower_file.c_str(), 06777)) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (setgid(1000) != 0) {
            _exit(2);
        }
        if (setuid(1000) != 0) {
            _exit(3);
        }

        int fd = open(merged_file.c_str(), O_WRONLY | O_TRUNC | O_CLOEXEC);
        if (fd < 0) {
            _exit(4);
        }
        _exit(close(fd) == 0 ? 0 : 5);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));

    struct stat st = {};
    ASSERT_EQ(0, stat(upper_file.c_str(), &st)) << strerror(errno);
    EXPECT_EQ(0, st.st_size);
    EXPECT_EQ(0u, static_cast<unsigned>(st.st_mode & (S_ISUID | S_ISGID)));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, CopyUpLargeLowerFileConcurrentReadersNeverSeePartialUpper) {
    constexpr size_t kFileSize = 4 * 1024 * 1024;
    ScopedOverlayEnv scoped("overlayfs_copy_up_concurrent");
    const auto& env = scoped.env;
    std::string lower_file = join_path(env.lower, "large");
    std::string upper_file = join_path(env.upper, "large");
    std::string merged_file = join_path(env.merged, "large");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_TRUE(write_pattern_file(lower_file, kFileSize)) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    int start_pipe[2] = {};
    int done_pipe[2] = {};
    ASSERT_EQ(0, pipe(start_pipe)) << strerror(errno);
    ASSERT_EQ(0, pipe(done_pipe)) << strerror(errno);

    pid_t reader = fork();
    ASSERT_GE(reader, 0) << strerror(errno);
    if (reader == 0) {
        alarm(10);
        close(start_pipe[1]);
        close(done_pipe[1]);
        char token = 0;
        if (read(start_pipe[0], &token, 1) != 1) {
            _exit(3);
        }
        fcntl(done_pipe[0], F_SETFL, O_NONBLOCK);
        for (;;) {
            if (!validate_pattern_file(merged_file, kFileSize)) {
                _exit(2);
            }
            char done = 0;
            ssize_t n = read(done_pipe[0], &done, 1);
            if (n == 0 || n == 1) {
                break;
            }
            if (n < 0 && errno != EAGAIN && errno != EWOULDBLOCK) {
                _exit(4);
            }
        }
        _exit(0);
    }

    close(start_pipe[0]);
    close(done_pipe[0]);
    ASSERT_EQ(1, write(start_pipe[1], "x", 1)) << strerror(errno);
    close(start_pipe[1]);
    int fd = open(merged_file.c_str(), O_WRONLY | O_CLOEXEC);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);
    close(done_pipe[1]);

    int status = 0;
    ASSERT_EQ(reader, waitpid(reader, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status)) << "reader status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_TRUE(validate_pattern_file(upper_file, kFileSize));
    EXPECT_TRUE(validate_pattern_file(merged_file, kFileSize));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(RenameAt2Semantics, RejectsUnknownWhiteoutAndInvalidFlagCombinations) {
    std::string root = std::string("/tmp/renameat2_flags_") + std::to_string(getpid());
    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(root.c_str()));
    std::string old_path = join_path(root, "old");
    std::string new_path = join_path(root, "new");

    ASSERT_EQ(0, write_text(old_path, "old"));
    errno = 0;
    EXPECT_EQ(-1, renameat2_call(old_path, new_path, 0x80000000U));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ("old", read_text(old_path));
    EXPECT_FALSE(path_exists(new_path));

    errno = 0;
    EXPECT_EQ(-1, renameat2_call(old_path, new_path, 0x80000000U | RENAME_NOREPLACE));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ("old", read_text(old_path));
    EXPECT_FALSE(path_exists(new_path));

    errno = 0;
    EXPECT_EQ(-1, renameat2_call(join_path(root, "missing"), new_path, 0x80000000U));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ("old", read_text(old_path));
    EXPECT_FALSE(path_exists(new_path));

    ASSERT_EQ(0, write_text(new_path, "new"));
    errno = 0;
    EXPECT_EQ(-1, renameat2_call(old_path, new_path, RENAME_EXCHANGE | RENAME_NOREPLACE));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ("old", read_text(old_path));
    EXPECT_EQ("new", read_text(new_path));

    errno = 0;
    EXPECT_EQ(-1, renameat2_call(old_path, new_path, RENAME_EXCHANGE | RENAME_WHITEOUT));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ("old", read_text(old_path));
    EXPECT_EQ("new", read_text(new_path));

    errno = 0;
    EXPECT_EQ(-1, renameat2_call(old_path, old_path, RENAME_NOREPLACE));
    EXPECT_EQ(EEXIST, errno);
    EXPECT_EQ("old", read_text(old_path));

    remove_recursive(root);
}

TEST(RenameAt2Semantics, WhiteoutRenamesAndLeavesCharZeroZero) {
    std::string root = std::string("/tmp/renameat2_whiteout_") + std::to_string(getpid());
    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(root.c_str()));
    if (mount("tmpfs", root.c_str(), "tmpfs", 0, "") != 0) {
        remove_recursive(root);
        GTEST_SKIP() << strerror(errno);
    }
    std::string old_path = join_path(root, "old");
    std::string new_path = join_path(root, "new");

    ASSERT_EQ(0, write_text(old_path, "old"));
    ASSERT_EQ(0, renameat2_call(old_path, new_path, RENAME_WHITEOUT)) << strerror(errno);

    EXPECT_TRUE(is_whiteout(old_path));
    EXPECT_EQ("old", read_text(new_path));
    umount(root.c_str());
    remove_recursive(root);
}

TEST(RenameAt2Semantics, TmpfsExchangeDirAndFileUpdatesParentNlink) {
    std::string root = std::string("/tmp/tmpfs_exchange_nlink_") + std::to_string(getpid());
    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(root.c_str()));
    if (mount("tmpfs", root.c_str(), "tmpfs", 0, "") != 0) {
        remove_recursive(root);
        GTEST_SKIP() << strerror(errno);
    }

    std::string a = join_path(root, "a");
    std::string b = join_path(root, "b");
    std::string dir = join_path(a, "dir");
    std::string file = join_path(b, "file");
    ASSERT_EQ(0, mkdir(a.c_str(), 0755));
    ASSERT_EQ(0, mkdir(b.c_str(), 0755));
    ASSERT_EQ(0, mkdir(dir.c_str(), 0755));
    ASSERT_EQ(0, write_text(file, "file"));

    struct stat a_before = {};
    struct stat b_before = {};
    ASSERT_EQ(0, stat(a.c_str(), &a_before)) << strerror(errno);
    ASSERT_EQ(0, stat(b.c_str(), &b_before)) << strerror(errno);

    ASSERT_EQ(0, renameat2_call(dir, file, RENAME_EXCHANGE)) << strerror(errno);

    struct stat a_after = {};
    struct stat b_after = {};
    ASSERT_EQ(0, stat(a.c_str(), &a_after)) << strerror(errno);
    ASSERT_EQ(0, stat(b.c_str(), &b_after)) << strerror(errno);
    EXPECT_EQ(a_before.st_nlink - 1, a_after.st_nlink);
    EXPECT_EQ(b_before.st_nlink + 1, b_after.st_nlink);
    EXPECT_EQ("file", read_text(join_path(a, "dir")));
    struct stat moved_dir = {};
    ASSERT_EQ(0, stat(join_path(b, "file").c_str(), &moved_dir)) << strerror(errno);
    EXPECT_TRUE(S_ISDIR(moved_dir.st_mode));

    umount(root.c_str());
    remove_recursive(root);
}

TEST(RenameAt2Semantics, TmpfsSameDirDirReplaceUpdatesParentNlink) {
    std::string root = std::string("/tmp/tmpfs_dir_replace_nlink_") + std::to_string(getpid());
    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(root.c_str()));
    if (mount("tmpfs", root.c_str(), "tmpfs", 0, "") != 0) {
        remove_recursive(root);
        GTEST_SKIP() << strerror(errno);
    }

    std::string old_dir = join_path(root, "old");
    std::string new_dir = join_path(root, "new");
    ASSERT_EQ(0, mkdir(old_dir.c_str(), 0755));
    ASSERT_EQ(0, mkdir(new_dir.c_str(), 0755));
    struct stat before = {};
    ASSERT_EQ(0, stat(root.c_str(), &before)) << strerror(errno);

    ASSERT_EQ(0, rename(old_dir.c_str(), new_dir.c_str())) << strerror(errno);

    struct stat after = {};
    ASSERT_EQ(0, stat(root.c_str(), &after)) << strerror(errno);
    EXPECT_EQ(before.st_nlink - 1, after.st_nlink);
    EXPECT_FALSE(path_exists(old_dir));
    struct stat moved = {};
    ASSERT_EQ(0, stat(new_dir.c_str(), &moved)) << strerror(errno);
    EXPECT_TRUE(S_ISDIR(moved.st_mode));

    umount(root.c_str());
    remove_recursive(root);
}

TEST(RenameAt2Semantics, TmpfsExchangeAncestorDirectoryReturnsEinval) {
    std::string root = std::string("/tmp/tmpfs_exchange_ancestor_") + std::to_string(getpid());
    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(root.c_str()));
    if (mount("tmpfs", root.c_str(), "tmpfs", 0, "") != 0) {
        remove_recursive(root);
        GTEST_SKIP() << strerror(errno);
    }

    std::string a = join_path(root, "a");
    std::string b = join_path(a, "b");
    std::string child = join_path(b, "child");
    ASSERT_EQ(0, mkdir(a.c_str(), 0755));
    ASSERT_EQ(0, mkdir(b.c_str(), 0755));
    ASSERT_EQ(0, write_text(child, "child"));

    errno = 0;
    EXPECT_EQ(-1, renameat2_call(b, a, RENAME_EXCHANGE));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_TRUE(path_exists(a));
    EXPECT_TRUE(path_exists(b));
    EXPECT_EQ("child", read_text(child));

    umount(root.c_str());
    remove_recursive(root);
}

TEST(OverlayFsSemantics, UpperOnlyRenameMovesEntry) {
    auto env = make_overlay_env("overlayfs_rename_upper");
    std::string upper_old = join_path(env.upper, "old");
    std::string upper_new = join_path(env.upper, "new");
    std::string merged_old = join_path(env.merged, "old");
    std::string merged_new = join_path(env.merged, "new");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(upper_old, "upper-old"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);
    ASSERT_EQ(0, rename(merged_old.c_str(), merged_new.c_str())) << strerror(errno);

    EXPECT_FALSE(path_exists(merged_old));
    EXPECT_EQ("upper-old", read_text(merged_new));
    EXPECT_FALSE(path_exists(upper_old));
    EXPECT_EQ("upper-old", read_text(upper_new));
    cleanup_overlay_env(env);
}

TEST(OverlayFsSemantics, UpperOnlyRenameNoReplacePreservesState) {
    auto env = make_overlay_env("overlayfs_rename_noreplace");
    std::string upper_old = join_path(env.upper, "old");
    std::string upper_new = join_path(env.upper, "new");
    std::string merged_old = join_path(env.merged, "old");
    std::string merged_new = join_path(env.merged, "new");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(upper_old, "upper-old"));
    ASSERT_EQ(0, write_text(upper_new, "upper-new"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, renameat2_call(merged_old, merged_new, RENAME_NOREPLACE));
    EXPECT_EQ(EEXIST, errno);

    EXPECT_EQ("upper-old", read_text(upper_old));
    EXPECT_EQ("upper-new", read_text(upper_new));
    EXPECT_EQ("upper-old", read_text(merged_old));
    EXPECT_EQ("upper-new", read_text(merged_new));
    cleanup_overlay_env(env);
}

TEST(OverlayFsSemantics, UserWhiteoutRenameIsRejected) {
    auto env = make_overlay_env("overlayfs_user_whiteout_reject");
    std::string upper_old = join_path(env.upper, "old");
    std::string merged_old = join_path(env.merged, "old");
    std::string merged_new = join_path(env.merged, "new");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(upper_old, "upper-old"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, renameat2_call(merged_old, merged_new, RENAME_WHITEOUT));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ("upper-old", read_text(merged_old));
    EXPECT_FALSE(path_exists(merged_new));
    cleanup_overlay_env(env);
}

TEST(OverlayFsSemantics, ExchangeCopiesUpLowerTarget) {
    auto env = make_overlay_env("overlayfs_exchange_lower_target");
    std::string upper_old = join_path(env.upper, "old");
    std::string upper_new = join_path(env.upper, "new");
    std::string lower_new = join_path(env.lower, "new");
    std::string merged_old = join_path(env.merged, "old");
    std::string merged_new = join_path(env.merged, "new");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(upper_old, "upper-old"));
    ASSERT_EQ(0, write_text(lower_new, "lower-new"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    ASSERT_EQ(0, renameat2_call(merged_old, merged_new, RENAME_EXCHANGE)) << strerror(errno);

    EXPECT_EQ("lower-new", read_text(merged_old));
    EXPECT_EQ("upper-old", read_text(merged_new));
    EXPECT_EQ("lower-new", read_text(upper_old));
    EXPECT_EQ("upper-old", read_text(upper_new));
    EXPECT_EQ("lower-new", read_text(lower_new));
    cleanup_overlay_env(env);
}

TEST(OverlayFsSemantics, LowerOnlyFileRenameCopiesUpAndWhiteoutsOldPath) {
    auto env = make_overlay_env("overlayfs_lower_rename_whiteout");
    std::string lower_old = join_path(env.lower, "old");
    std::string upper_old = join_path(env.upper, "old");
    std::string upper_new = join_path(env.upper, "new");
    std::string merged_old = join_path(env.merged, "old");
    std::string merged_new = join_path(env.merged, "new");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(lower_old, "lower-old"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    ASSERT_EQ(0, rename(merged_old.c_str(), merged_new.c_str())) << strerror(errno);

    EXPECT_EQ("lower-old", read_text(lower_old));
    EXPECT_FALSE(path_exists(merged_old));
    EXPECT_EQ("lower-old", read_text(merged_new));
    EXPECT_TRUE(is_whiteout(upper_old));
    EXPECT_EQ("lower-old", read_text(upper_new));
    cleanup_overlay_env(env);
}

TEST(OverlayFsSemantics, RenameNoReplaceOverWhiteoutTargetTreatsTargetAsAbsent) {
    auto env = make_overlay_env("overlayfs_rename_over_whiteout");
    std::string upper_old = join_path(env.upper, "old");
    std::string upper_new = join_path(env.upper, "new");
    std::string lower_new = join_path(env.lower, "new");
    std::string merged_old = join_path(env.merged, "old");
    std::string merged_new = join_path(env.merged, "new");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(upper_old, "upper-old"));
    ASSERT_EQ(0, write_text(lower_new, "lower-new"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);
    ASSERT_EQ(0, unlink(merged_new.c_str())) << strerror(errno);
    ASSERT_TRUE(is_whiteout(upper_new));
    EXPECT_FALSE(path_exists(merged_new));

    ASSERT_EQ(0, renameat2_call(merged_old, merged_new, RENAME_NOREPLACE)) << strerror(errno);
    EXPECT_FALSE(path_exists(merged_old));
    EXPECT_EQ("upper-old", read_text(merged_new));
    EXPECT_FALSE(is_whiteout(upper_new));
    EXPECT_EQ("upper-old", read_text(upper_new));
    EXPECT_EQ("lower-new", read_text(lower_new));
    cleanup_overlay_env(env);
}

TEST(OverlayFsSemantics, LowerFileRenameToDirectoryFailsWithoutCopyUp) {
    auto env = make_overlay_env("overlayfs_lower_file_to_dir_no_copyup");
    std::string lower_old = join_path(env.lower, "old");
    std::string upper_old = join_path(env.upper, "old");
    std::string upper_new = join_path(env.upper, "new");
    std::string merged_old = join_path(env.merged, "old");
    std::string merged_new = join_path(env.merged, "new");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(lower_old, "lower-old"));
    ASSERT_EQ(0, mkdir(upper_new.c_str(), 0755));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, rename(merged_old.c_str(), merged_new.c_str()));
    EXPECT_EQ(EISDIR, errno);
    EXPECT_EQ("lower-old", read_text(merged_old));
    EXPECT_TRUE(path_exists(merged_new));
    EXPECT_FALSE(path_exists(upper_old));
    cleanup_overlay_env(env);
}

TEST(OverlayFsSemantics, ExchangeLowerDirReturnsExdevNoUpperHalfMove) {
    auto env = make_overlay_env("overlayfs_exchange_lower_dir");
    std::string lower_old = join_path(env.lower, "old");
    std::string lower_child = join_path(lower_old, "child");
    std::string upper_old = join_path(env.upper, "old");
    std::string upper_new = join_path(env.upper, "new");
    std::string merged_old = join_path(env.merged, "old");
    std::string merged_new = join_path(env.merged, "new");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, mkdir(lower_old.c_str(), 0755));
    ASSERT_EQ(0, write_text(lower_child, "child"));
    ASSERT_EQ(0, write_text(upper_new, "upper-new"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, renameat2_call(merged_old, merged_new, RENAME_EXCHANGE));
    EXPECT_EQ(EXDEV, errno);

    EXPECT_FALSE(path_exists(upper_old));
    EXPECT_EQ("upper-new", read_text(upper_new));
    EXPECT_EQ("child", read_text(lower_child));
    EXPECT_EQ("child", read_text(join_path(merged_old, "child")));
    EXPECT_EQ("upper-new", read_text(merged_new));
    cleanup_overlay_env(env);
}

TEST(OverlayFsSemantics, UpperDirRenameOverNonEmptyLowerDirReturnsEnotempty) {
    auto env = make_overlay_env("overlayfs_upper_dir_over_nonempty_lower_dir");
    std::string upper_old = join_path(env.upper, "old");
    std::string upper_new = join_path(env.upper, "new");
    std::string lower_new = join_path(env.lower, "new");
    std::string lower_child = join_path(lower_new, "child");
    std::string merged_old = join_path(env.merged, "old");
    std::string merged_new = join_path(env.merged, "new");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, mkdir(upper_old.c_str(), 0755));
    ASSERT_EQ(0, mkdir(lower_new.c_str(), 0755));
    ASSERT_EQ(0, write_text(lower_child, "child"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, rename(merged_old.c_str(), merged_new.c_str()));
    EXPECT_EQ(ENOTEMPTY, errno);

    EXPECT_TRUE(path_exists(upper_old));
    EXPECT_FALSE(path_exists(upper_new));
    EXPECT_EQ("child", read_text(lower_child));
    EXPECT_TRUE(path_exists(merged_old));
    EXPECT_EQ("child", read_text(join_path(merged_new, "child")));
    cleanup_overlay_env(env);
}

TEST(OverlayFsSemantics, UpperDirRenameOverEmptyLowerDirSucceeds) {
    auto env = make_overlay_env("overlayfs_upper_dir_over_empty_lower_dir");
    std::string upper_old = join_path(env.upper, "old");
    std::string upper_new = join_path(env.upper, "new");
    std::string lower_new = join_path(env.lower, "new");
    std::string merged_old = join_path(env.merged, "old");
    std::string merged_new = join_path(env.merged, "new");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, mkdir(upper_old.c_str(), 0755));
    ASSERT_EQ(0, mkdir(lower_new.c_str(), 0755));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    ASSERT_EQ(0, rename(merged_old.c_str(), merged_new.c_str())) << strerror(errno);

    EXPECT_FALSE(path_exists(merged_old));
    EXPECT_TRUE(path_exists(merged_new));
    EXPECT_FALSE(path_exists(upper_old));
    EXPECT_TRUE(path_exists(upper_new));
    cleanup_overlay_env(env);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
