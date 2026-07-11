#include <gtest/gtest.h>

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <signal.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/sysmacros.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <algorithm>
#include <string>
#include <utility>
#include <vector>
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

std::vector<std::string> directory_names(const std::string& path) {
    std::vector<std::string> names;
    DIR* dir = opendir(path.c_str());
    if (dir == nullptr) {
        return names;
    }
    while (dirent* ent = readdir(dir)) {
        if (strcmp(ent->d_name, ".") != 0 && strcmp(ent->d_name, "..") != 0) {
            names.emplace_back(ent->d_name);
        }
    }
    closedir(dir);
    std::sort(names.begin(), names.end());
    return names;
}

std::vector<std::string> xattr_names(const std::string& path) {
    ssize_t needed = listxattr(path.c_str(), nullptr, 0);
    EXPECT_GE(needed, 0) << path << ": " << strerror(errno);
    if (needed <= 0) {
        return {};
    }

    std::vector<char> list(static_cast<size_t>(needed));
    ssize_t actual = listxattr(path.c_str(), list.data(), list.size());
    EXPECT_EQ(needed, actual) << path << ": " << strerror(errno);
    if (actual != needed) {
        return {};
    }

    std::vector<std::string> names;
    size_t start = 0;
    for (size_t i = 0; i < static_cast<size_t>(actual); ++i) {
        if (list[i] == '\0') {
            names.emplace_back(list.data() + start, i - start);
            start = i + 1;
        }
    }
    EXPECT_EQ(static_cast<size_t>(actual), start);
    return names;
}

bool has_xattr_name(const std::vector<std::string>& names, const char* name) {
    return std::find(names.begin(), names.end(), name) != names.end();
}

void expect_xattr(const std::string& path, const char* name, const void* expected, size_t size) {
    std::vector<unsigned char> value(size == 0 ? 1 : size);
    ssize_t actual = getxattr(path.c_str(), name, value.data(), value.size());
    ASSERT_EQ(static_cast<ssize_t>(size), actual)
        << path << " " << name << ": " << strerror(errno);
    EXPECT_EQ(0, memcmp(value.data(), expected, size));
}

void expect_path_enoent(const std::string& path) {
    struct stat st = {};
    errno = 0;
    EXPECT_EQ(-1, lstat(path.c_str(), &st));
    EXPECT_EQ(ENOENT, errno) << path << ": " << strerror(errno);
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

std::string read_symlink_target(const std::string& path) {
    char buf[512] = {};
    ssize_t n = readlink(path.c_str(), buf, sizeof(buf) - 1);
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

OverlayRenameEnv make_overlay_env(const char* name, const char* base = "/tmp") {
    std::string root = std::string(base) + "/" + name + "_" + std::to_string(getpid());
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
    explicit ScopedOverlayEnv(const char* name, const char* base = "/tmp")
        : env(make_overlay_env(name, base)) {}

    ~ScopedOverlayEnv() {
        cleanup_overlay_env(env);
    }

    OverlayRenameEnv env;
};

bool root_supports_ext4_xattrs() {
    struct statfs st = {};
    return statfs("/root", &st) == 0 && st.f_type == 0xEF53;
}

bool is_xattr_unsupported_errno(int error) {
    return error == ENOSYS || error == EOPNOTSUPP;
}

struct ScopedCustomMount {
    ScopedCustomMount(std::string root_path, std::string merged_path)
        : root(std::move(root_path)), merged(std::move(merged_path)) {}

    ~ScopedCustomMount() {
        if (mounted) {
            umount(merged.c_str());
        }
        remove_recursive(root);
    }

    std::string root;
    std::string merged;
    bool mounted = false;
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

void prepare_overlay_env(const OverlayRenameEnv& env) {
    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
}

void expect_truncate_copy_up_drops_privileged_bits(const char* name, const char* lower_text) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root to prepare setuid/setgid lower file";
    }

    ScopedOverlayEnv scoped(name);
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
    ASSERT_EQ(0, write_text(lower_file, lower_text));
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

TEST(OverlayFsSemantics, MknodFifoOverWhiteoutCreatesReachableUpperNode) {
    ScopedOverlayEnv scoped("overlayfs_mknod_fifo_whiteout");
    const auto& env = scoped.env;
    std::string lower_entry = join_path(env.lower, "entry");
    std::string upper_entry = join_path(env.upper, "entry");
    std::string merged_entry = join_path(env.merged, "entry");

    prepare_overlay_env(env);
    ASSERT_EQ(0, write_text(lower_entry, "lower"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    ASSERT_EQ(0, unlink(merged_entry.c_str())) << strerror(errno);
    ASSERT_TRUE(is_whiteout(upper_entry));
    ASSERT_EQ(0, mkfifo(merged_entry.c_str(), 0640)) << strerror(errno);

    struct stat merged_st = {};
    struct stat upper_st = {};
    struct stat lower_st = {};
    ASSERT_EQ(0, lstat(merged_entry.c_str(), &merged_st)) << strerror(errno);
    ASSERT_EQ(0, lstat(upper_entry.c_str(), &upper_st)) << strerror(errno);
    ASSERT_EQ(0, lstat(lower_entry.c_str(), &lower_st)) << strerror(errno);
    EXPECT_TRUE(S_ISFIFO(merged_st.st_mode));
    EXPECT_TRUE(S_ISFIFO(upper_st.st_mode));
    EXPECT_TRUE(S_ISREG(lower_st.st_mode));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
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

TEST(OverlayFsSemantics, OldLowerFileDescriptorsSwitchToUpperAfterCopyUp) {
    ScopedOverlayEnv scoped("overlayfs_revalidate_old_fds");
    const auto& env = scoped.env;
    std::string lower_file = join_path(env.lower, "file");
    std::string merged_file = join_path(env.merged, "file");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(lower_file, "lower"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    int old_fd1 = open(merged_file.c_str(), O_RDONLY | O_CLOEXEC);
    ASSERT_GE(old_fd1, 0) << strerror(errno);
    int old_fd2 = open(merged_file.c_str(), O_RDONLY | O_CLOEXEC);
    ASSERT_GE(old_fd2, 0) << strerror(errno);

    ASSERT_EQ(0, write_text(merged_file, "upper")) << strerror(errno);
    for (int fd : {old_fd1, old_fd2}) {
        char buf[8] = {};
        ASSERT_EQ(5, pread(fd, buf, 5, 0)) << strerror(errno);
        EXPECT_EQ("upper", std::string(buf, 5));
    }

    ASSERT_EQ(0, write_text(merged_file, "newer")) << strerror(errno);
    char buf[8] = {};
    ASSERT_EQ(5, pread(old_fd1, buf, 5, 0)) << strerror(errno);
    EXPECT_EQ("newer", std::string(buf, 5));
    EXPECT_EQ(0, fdatasync(old_fd1)) << strerror(errno);

    EXPECT_EQ(0, close(old_fd2)) << strerror(errno);
    EXPECT_EQ(0, close(old_fd1)) << strerror(errno);
}

TEST(OverlayFsSemantics, OldLowerFileDescriptorDoesNotFollowSameNameReplacement) {
    ScopedOverlayEnv scoped("overlayfs_revalidate_identity");
    const auto& env = scoped.env;
    std::string lower_file = join_path(env.lower, "file");
    std::string merged_file = join_path(env.merged, "file");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(lower_file, "lower"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    int old_fd = open(merged_file.c_str(), O_RDONLY | O_CLOEXEC);
    ASSERT_GE(old_fd, 0) << strerror(errno);
    ASSERT_EQ(0, unlink(merged_file.c_str())) << strerror(errno);
    ASSERT_EQ(0, write_text(merged_file, "other")) << strerror(errno);
    EXPECT_EQ("other", read_text(merged_file));

    char buf[8] = {};
    ASSERT_EQ(5, pread(old_fd, buf, 5, 0)) << strerror(errno);
    EXPECT_EQ("lower", std::string(buf, 5));
    EXPECT_EQ(0, close(old_fd)) << strerror(errno);
}

TEST(OverlayFsSemantics, NewMmapFromOldLowerFdUsesUpperSnapshot) {
    ScopedOverlayEnv scoped("overlayfs_revalidate_mmap");
    const auto& env = scoped.env;
    std::string lower_file = join_path(env.lower, "file");
    std::string merged_file = join_path(env.merged, "file");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(lower_file, "lower"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    int old_fd = open(merged_file.c_str(), O_RDONLY | O_CLOEXEC);
    ASSERT_GE(old_fd, 0) << strerror(errno);
    ASSERT_EQ(0, write_text(merged_file, "upper")) << strerror(errno);

    void* mapping = mmap(nullptr, 4096, PROT_READ, MAP_PRIVATE, old_fd, 0);
    ASSERT_NE(MAP_FAILED, mapping) << strerror(errno);
    EXPECT_EQ("upper", std::string(static_cast<const char*>(mapping), 5));
    EXPECT_EQ(0, munmap(mapping, 4096)) << strerror(errno);
    EXPECT_EQ(0, close(old_fd)) << strerror(errno);
}

TEST(OverlayFsSemantics, LowerHardlinkRedirectsKeepIndependentCopyUpState) {
    ScopedOverlayEnv scoped("overlayfs_revalidate_hardlinks");
    const auto& env = scoped.env;
    std::string lower_a = join_path(env.lower, "a");
    std::string lower_b = join_path(env.lower, "b");
    std::string merged_a = join_path(env.merged, "a");
    std::string merged_b = join_path(env.merged, "b");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(lower_a, "lower"));
    ASSERT_EQ(0, link(lower_a.c_str(), lower_b.c_str())) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    int old_a = open(merged_a.c_str(), O_RDONLY | O_CLOEXEC);
    ASSERT_GE(old_a, 0) << strerror(errno);
    int old_b = open(merged_b.c_str(), O_RDONLY | O_CLOEXEC);
    ASSERT_GE(old_b, 0) << strerror(errno);
    ASSERT_EQ(0, write_text(merged_a, "upper")) << strerror(errno);

    char a_buf[8] = {};
    char b_buf[8] = {};
    ASSERT_EQ(5, pread(old_a, a_buf, 5, 0)) << strerror(errno);
    ASSERT_EQ(5, pread(old_b, b_buf, 5, 0)) << strerror(errno);
    EXPECT_EQ("upper", std::string(a_buf, 5));
    EXPECT_EQ("lower", std::string(b_buf, 5));
    EXPECT_EQ(0, close(old_b)) << strerror(errno);
    EXPECT_EQ(0, close(old_a)) << strerror(errno);
}

TEST(OverlayFsSemantics, HardlinkUsesRealUpperInodeForLowerAndUpperSources) {
    ScopedOverlayEnv scoped("overlayfs_hardlink_real_upper");
    const auto& env = scoped.env;
    std::string lower_a = join_path(env.lower, "a");
    std::string merged_a = join_path(env.merged, "a");
    std::string merged_b = join_path(env.merged, "b");
    std::string upper_a = join_path(env.upper, "a");
    std::string upper_b = join_path(env.upper, "b");
    std::string merged_c = join_path(env.merged, "c");
    std::string merged_d = join_path(env.merged, "d");
    std::string upper_c = join_path(env.upper, "c");
    std::string upper_d = join_path(env.upper, "d");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(lower_a, "lower"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    ASSERT_EQ(0, link(merged_a.c_str(), merged_b.c_str())) << strerror(errno);

    struct stat merged_a_st = {};
    struct stat merged_b_st = {};
    struct stat upper_a_st = {};
    struct stat upper_b_st = {};
    ASSERT_EQ(0, stat(merged_a.c_str(), &merged_a_st)) << strerror(errno);
    ASSERT_EQ(0, stat(merged_b.c_str(), &merged_b_st)) << strerror(errno);
    ASSERT_EQ(0, stat(upper_a.c_str(), &upper_a_st)) << strerror(errno);
    ASSERT_EQ(0, stat(upper_b.c_str(), &upper_b_st)) << strerror(errno);
    EXPECT_EQ(merged_a_st.st_ino, merged_b_st.st_ino);
    EXPECT_EQ(upper_a_st.st_ino, upper_b_st.st_ino);
    EXPECT_EQ(2u, static_cast<unsigned>(merged_a_st.st_nlink));
    EXPECT_EQ(2u, static_cast<unsigned>(upper_b_st.st_nlink));

    ASSERT_EQ(0, write_text(merged_b, "lower-linked")) << strerror(errno);
    EXPECT_EQ("lower-linked", read_text(merged_a));
    EXPECT_EQ("lower-linked", read_text(upper_a));

    ASSERT_EQ(0, write_text(merged_c, "upper")) << strerror(errno);
    ASSERT_EQ(0, link(merged_c.c_str(), merged_d.c_str())) << strerror(errno);

    struct stat merged_c_st = {};
    struct stat merged_d_st = {};
    struct stat upper_c_st = {};
    struct stat upper_d_st = {};
    ASSERT_EQ(0, stat(merged_c.c_str(), &merged_c_st)) << strerror(errno);
    ASSERT_EQ(0, stat(merged_d.c_str(), &merged_d_st)) << strerror(errno);
    ASSERT_EQ(0, stat(upper_c.c_str(), &upper_c_st)) << strerror(errno);
    ASSERT_EQ(0, stat(upper_d.c_str(), &upper_d_st)) << strerror(errno);
    EXPECT_EQ(merged_c_st.st_ino, merged_d_st.st_ino);
    EXPECT_EQ(merged_c_st.st_ino, upper_c_st.st_ino);
    EXPECT_EQ(upper_c_st.st_ino, upper_d_st.st_ino);
    EXPECT_EQ(2u, static_cast<unsigned>(merged_c_st.st_nlink));

    ASSERT_EQ(0, write_text(merged_d, "upper-linked")) << strerror(errno);
    EXPECT_EQ("upper-linked", read_text(merged_c));
    EXPECT_EQ("upper-linked", read_text(upper_c));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, HardlinkCopyUpPreservesLowerOwnershipAndMode) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root to prepare lower ownership";
    }

    ScopedOverlayEnv scoped("overlayfs_hardlink_copy_up_metadata");
    const auto& env = scoped.env;
    std::string lower_source = join_path(env.lower, "source");
    std::string merged_source = join_path(env.merged, "source");
    std::string merged_target = join_path(env.merged, "target");
    std::string upper_source = join_path(env.upper, "source");
    std::string upper_target = join_path(env.upper, "target");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(lower_source, "lower-owned"));
    ASSERT_EQ(0, chown(lower_source.c_str(), 1000, 1001)) << strerror(errno);
    ASSERT_EQ(0, chmod(lower_source.c_str(), 0640)) << strerror(errno);

    struct stat lower_st = {};
    ASSERT_EQ(0, stat(lower_source.c_str(), &lower_st)) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    ASSERT_EQ(0, link(merged_source.c_str(), merged_target.c_str())) << strerror(errno);

    struct stat merged_source_st = {};
    struct stat merged_target_st = {};
    struct stat upper_source_st = {};
    struct stat upper_target_st = {};
    ASSERT_EQ(0, stat(merged_source.c_str(), &merged_source_st)) << strerror(errno);
    ASSERT_EQ(0, stat(merged_target.c_str(), &merged_target_st)) << strerror(errno);
    ASSERT_EQ(0, stat(upper_source.c_str(), &upper_source_st)) << strerror(errno);
    ASSERT_EQ(0, stat(upper_target.c_str(), &upper_target_st)) << strerror(errno);

    EXPECT_EQ(lower_st.st_uid, merged_source_st.st_uid);
    EXPECT_EQ(lower_st.st_gid, merged_source_st.st_gid);
    EXPECT_EQ(lower_st.st_mode & 07777, merged_source_st.st_mode & 07777);
    EXPECT_EQ(lower_st.st_uid, merged_target_st.st_uid);
    EXPECT_EQ(lower_st.st_gid, merged_target_st.st_gid);
    EXPECT_EQ(lower_st.st_mode & 07777, merged_target_st.st_mode & 07777);
    EXPECT_EQ(lower_st.st_uid, upper_source_st.st_uid);
    EXPECT_EQ(lower_st.st_gid, upper_source_st.st_gid);
    EXPECT_EQ(lower_st.st_mode & 07777, upper_source_st.st_mode & 07777);
    EXPECT_EQ(lower_st.st_uid, upper_target_st.st_uid);
    EXPECT_EQ(lower_st.st_gid, upper_target_st.st_gid);
    EXPECT_EQ(lower_st.st_mode & 07777, upper_target_st.st_mode & 07777);
    EXPECT_EQ(upper_source_st.st_ino, upper_target_st.st_ino);
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, HardlinkExistingMergedTargetFailsBeforeSourceCopyUp) {
    ScopedOverlayEnv scoped("overlayfs_hardlink_existing_target");
    const auto& env = scoped.env;
    std::string lower_source = join_path(env.lower, "source");
    std::string lower_target = join_path(env.lower, "target");
    std::string upper_source = join_path(env.upper, "source");
    std::string upper_target = join_path(env.upper, "target");
    std::string merged_source = join_path(env.merged, "source");
    std::string merged_target = join_path(env.merged, "target");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(lower_source, "source"));
    ASSERT_EQ(0, write_text(lower_target, "lower-target"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, link(merged_source.c_str(), merged_target.c_str()));
    EXPECT_EQ(EEXIST, errno);
    EXPECT_FALSE(path_exists(upper_source));
    EXPECT_FALSE(path_exists(upper_target));
    EXPECT_EQ("source", read_text(merged_source));
    EXPECT_EQ("lower-target", read_text(merged_target));

    struct stat lower_source_st = {};
    ASSERT_EQ(0, stat(lower_source.c_str(), &lower_source_st)) << strerror(errno);
    EXPECT_EQ(1u, static_cast<unsigned>(lower_source_st.st_nlink));

    ASSERT_EQ(0, write_text(upper_target, "upper-target"));
    errno = 0;
    EXPECT_EQ(-1, link(merged_source.c_str(), merged_target.c_str()));
    EXPECT_EQ(EEXIST, errno);
    EXPECT_FALSE(path_exists(upper_source));
    EXPECT_EQ("upper-target", read_text(merged_target));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, HardlinkMayCreatePermissionAndErrorOrdering) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root to prepare a non-owner caller";
    }

    ScopedOverlayEnv scoped("overlayfs_hardlink_permission");
    const auto& env = scoped.env;
    std::string lower_source = join_path(env.lower, "source");
    std::string lower_existing = join_path(env.lower, "existing");
    std::string upper_source = join_path(env.upper, "source");
    std::string merged_source = join_path(env.merged, "source");
    std::string merged_existing = join_path(env.merged, "existing");
    std::string merged_missing = join_path(env.merged, "missing");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(lower_source, "source"));
    ASSERT_EQ(0, write_text(lower_existing, "existing"));
    ASSERT_EQ(0, chown(lower_source.c_str(), 1000, 1000)) << strerror(errno);
    ASSERT_EQ(0, chmod(env.upper.c_str(), 0555)) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (setgid(1000) != 0 || setuid(1000) != 0) {
            _exit(2);
        }

        errno = 0;
        if (link(merged_source.c_str(), merged_existing.c_str()) != -1 || errno != EEXIST) {
            _exit(3);
        }

        errno = 0;
        if (link(merged_source.c_str(), merged_missing.c_str()) != -1 || errno != EACCES) {
            _exit(4);
        }
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_FALSE(path_exists(upper_source));
    EXPECT_FALSE(path_exists(merged_missing));
}

TEST(OverlayFsSemantics, HardlinkReadonlyMountAndExistingTargetErrorOrdering) {
    ScopedOverlayEnv scoped("overlayfs_hardlink_readonly");
    const auto& env = scoped.env;
    std::string lower_source = join_path(env.lower, "source");
    std::string lower_existing = join_path(env.lower, "existing");
    std::string merged_source = join_path(env.merged, "source");
    std::string merged_existing = join_path(env.merged, "existing");
    std::string merged_missing = join_path(env.merged, "missing");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(lower_source, "source"));
    ASSERT_EQ(0, write_text(lower_existing, "existing"));

    std::string options =
        "lowerdir=" + env.lower + ",upperdir=" + env.upper + ",workdir=" + env.work;
    ASSERT_EQ(0, mount("overlay", env.merged.c_str(), "overlay", MS_RDONLY, options.c_str()))
        << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, link(env.merged.c_str(), merged_existing.c_str()));
    EXPECT_EQ(EEXIST, errno);

    errno = 0;
    EXPECT_EQ(-1, link(merged_source.c_str(), merged_existing.c_str()));
    EXPECT_EQ(EEXIST, errno);

    errno = 0;
    EXPECT_EQ(-1, link(merged_source.c_str(), merged_missing.c_str()));
    EXPECT_EQ(EROFS, errno);
}

TEST(OverlayFsSemantics, HardlinkReplacesWhiteoutWithRealUpperInode) {
    ScopedOverlayEnv scoped("overlayfs_hardlink_whiteout");
    const auto& env = scoped.env;
    std::string lower_source = join_path(env.lower, "source");
    std::string lower_target = join_path(env.lower, "target");
    std::string upper_source = join_path(env.upper, "source");
    std::string upper_target = join_path(env.upper, "target");
    std::string merged_source = join_path(env.merged, "source");
    std::string merged_target = join_path(env.merged, "target");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(lower_source, "source"));
    ASSERT_EQ(0, write_text(lower_target, "lower-target"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    ASSERT_EQ(0, unlink(merged_target.c_str())) << strerror(errno);
    ASSERT_TRUE(is_whiteout(upper_target));
    ASSERT_EQ(0, link(merged_source.c_str(), merged_target.c_str())) << strerror(errno);
    EXPECT_FALSE(is_whiteout(upper_target));
    EXPECT_EQ("source", read_text(merged_target));
    EXPECT_EQ("lower-target", read_text(lower_target));

    struct stat source_st = {};
    struct stat target_st = {};
    ASSERT_EQ(0, stat(upper_source.c_str(), &source_st)) << strerror(errno);
    ASSERT_EQ(0, stat(upper_target.c_str(), &target_st)) << strerror(errno);
    EXPECT_EQ(source_st.st_ino, target_st.st_ino);
    EXPECT_EQ(2u, static_cast<unsigned>(source_st.st_nlink));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, LowerSymlinkHardlinkUsesRealUpperInode) {
    ScopedOverlayEnv scoped("overlayfs_hardlink_symlink");
    const auto& env = scoped.env;
    std::string lower_source = join_path(env.lower, "source");
    std::string merged_source = join_path(env.merged, "source");
    std::string merged_target = join_path(env.merged, "target");
    std::string upper_source = join_path(env.upper, "source");
    std::string upper_target = join_path(env.upper, "target");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, symlink("symlink-value", lower_source.c_str())) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    ASSERT_EQ(0, link(merged_source.c_str(), merged_target.c_str())) << strerror(errno);
    EXPECT_EQ("symlink-value", read_symlink_target(merged_source));
    EXPECT_EQ("symlink-value", read_symlink_target(merged_target));
    EXPECT_EQ("symlink-value", read_symlink_target(upper_source));
    EXPECT_EQ("symlink-value", read_symlink_target(upper_target));

    struct stat source_st = {};
    struct stat target_st = {};
    ASSERT_EQ(0, lstat(merged_source.c_str(), &source_st)) << strerror(errno);
    ASSERT_EQ(0, lstat(merged_target.c_str(), &target_st)) << strerror(errno);
    EXPECT_EQ(source_st.st_ino, target_st.st_ino);
    EXPECT_EQ(2u, static_cast<unsigned>(source_st.st_nlink));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
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
    expect_truncate_copy_up_drops_privileged_bits(
        "overlayfs_copy_up_truncate_mode",
        "truncate should clear privileged bits");
}

TEST(OverlayFsSemantics, CopyUpTruncateEmptyFileDropsPrivilegedBitsWithoutCapFsetid) {
    expect_truncate_copy_up_drops_privileged_bits("overlayfs_copy_up_truncate_empty_mode", "");
}

TEST(OverlayFsSemantics, CopyUpTruncateKeepsMandatoryLockingSgidForMember) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root to prepare setgid lower file";
    }

    ScopedOverlayEnv scoped("overlayfs_copy_up_truncate_keep_sgid");
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
    ASSERT_EQ(0, write_text(lower_file, "mandatory-locking-sgid"));
    ASSERT_EQ(0, chown(lower_file.c_str(), 1000, 1000)) << strerror(errno);
    ASSERT_EQ(0, chmod(lower_file.c_str(), 02660)) << strerror(errno);
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
    EXPECT_EQ(1000u, st.st_gid);
    EXPECT_NE(0u, static_cast<unsigned>(st.st_mode & S_ISGID));
}

TEST(OverlayFsSemantics, CopyUpTruncateExistingUpperStillTruncates) {
    ScopedOverlayEnv scoped("overlayfs_copy_up_truncate_existing_upper");
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
    ASSERT_EQ(0, write_text(lower_file, "lower-data"));
    ASSERT_EQ(0, write_text(upper_file, "existing-upper-data"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    int fd = open(merged_file.c_str(), O_WRONLY | O_TRUNC | O_CLOEXEC);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);

    struct stat st = {};
    ASSERT_EQ(0, stat(upper_file.c_str(), &st)) << strerror(errno);
    EXPECT_EQ(0, st.st_size);
    EXPECT_EQ("", read_text(upper_file));
    EXPECT_EQ("", read_text(merged_file));
    EXPECT_EQ("lower-data", read_text(lower_file));
}

TEST(OverlayFsSemantics, OpenLowerFifoWithTruncateDoesNotCopyUp) {
    ScopedOverlayEnv scoped("overlayfs_open_fifo_truncate");
    const auto& env = scoped.env;
    std::string lower_fifo = join_path(env.lower, "fifo");
    std::string upper_fifo = join_path(env.upper, "fifo");
    std::string merged_fifo = join_path(env.merged, "fifo");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, mkfifo(lower_fifo.c_str(), 0644)) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    int fd = open(merged_fifo.c_str(), O_RDONLY | O_TRUNC | O_NONBLOCK | O_CLOEXEC);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);

    EXPECT_FALSE(path_exists(upper_fifo));
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

TEST(OverlayFsSemantics, CopyUpLowerSymlinkPreservesTarget) {
    ScopedOverlayEnv scoped("overlayfs_copy_up_symlink");
    const auto& env = scoped.env;
    std::string lower_target = join_path(env.lower, "target");
    std::string lower_link = join_path(env.lower, "link");
    std::string upper_renamed = join_path(env.upper, "renamed-link");
    std::string merged_link = join_path(env.merged, "link");
    std::string merged_renamed = join_path(env.merged, "renamed-link");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(lower_target, "target-data"));
    ASSERT_EQ(0, symlink("target", lower_link.c_str())) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    ASSERT_EQ(0, rename(merged_link.c_str(), merged_renamed.c_str())) << strerror(errno);

    struct stat st = {};
    ASSERT_EQ(0, lstat(upper_renamed.c_str(), &st)) << strerror(errno);
    EXPECT_TRUE(S_ISLNK(st.st_mode));
    EXPECT_EQ("target", read_symlink_target(upper_renamed));
    EXPECT_EQ("target", read_symlink_target(merged_renamed));
    EXPECT_EQ("target-data", read_text(merged_renamed));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, ReloadCopiedUpExt4SymlinkWithoutOriginXattr) {
    if (!root_supports_ext4_xattrs()) {
        GTEST_SKIP() << "requires ext4 rootfs symlink xattr behavior";
    }

    ScopedOverlayEnv scoped("overlayfs_reload_ext4_symlink", "/root");
    const auto& env = scoped.env;
    std::string lower_target = join_path(env.lower, "target");
    std::string lower_link = join_path(env.lower, "link");
    std::string merged_link = join_path(env.merged, "link");
    std::string merged_renamed = join_path(env.merged, "renamed-link");

    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, write_text(lower_target, "target-data"));
    ASSERT_EQ(0, symlink("target", lower_link.c_str())) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);
    ASSERT_EQ(0, rename(merged_link.c_str(), merged_renamed.c_str())) << strerror(errno);

    // ext4 currently rejects xattrs on symlinks with EPERM. Remount to force
    // OverlayFS to instantiate a fresh inode and reload the optional origin
    // xattr instead of reusing the copy-up inode state.
    ASSERT_EQ(0, umount(env.merged.c_str())) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);
    struct stat st = {};
    ASSERT_EQ(0, lstat(merged_renamed.c_str(), &st)) << strerror(errno);
    EXPECT_TRUE(S_ISLNK(st.st_mode));
    EXPECT_EQ("target", read_symlink_target(merged_renamed));
    EXPECT_EQ("target-data", read_text(merged_renamed));
}

TEST(OverlayFsSemantics, CopyUpLowerCharDevicePreservesRdev) {
    ScopedOverlayEnv scoped("overlayfs_copy_up_chrdev");
    const auto& env = scoped.env;
    std::string lower_node = join_path(env.lower, "node");
    std::string upper_renamed = join_path(env.upper, "renamed-node");
    std::string merged_node = join_path(env.merged, "node");
    std::string merged_renamed = join_path(env.merged, "renamed-node");
    dev_t dev = makedev(1, 7);

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, mknod(lower_node.c_str(), S_IFCHR | 0600, dev)) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    ASSERT_EQ(0, rename(merged_node.c_str(), merged_renamed.c_str())) << strerror(errno);

    struct stat st = {};
    ASSERT_EQ(0, lstat(upper_renamed.c_str(), &st)) << strerror(errno);
    EXPECT_TRUE(S_ISCHR(st.st_mode));
    EXPECT_EQ(dev, st.st_rdev);
    EXPECT_FALSE(is_whiteout(upper_renamed));
    ASSERT_EQ(0, lstat(merged_renamed.c_str(), &st)) << strerror(errno);
    EXPECT_TRUE(S_ISCHR(st.st_mode));
    EXPECT_EQ(dev, st.st_rdev);
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, CopyUpLowerBlockDevicePreservesRdev) {
    ScopedOverlayEnv scoped("overlayfs_copy_up_blkdev");
    const auto& env = scoped.env;
    std::string lower_node = join_path(env.lower, "node");
    std::string upper_renamed = join_path(env.upper, "renamed-node");
    std::string merged_node = join_path(env.merged, "node");
    std::string merged_renamed = join_path(env.merged, "renamed-node");
    dev_t dev = makedev(7, 1);

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, mknod(lower_node.c_str(), S_IFBLK | 0600, dev)) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    ASSERT_EQ(0, rename(merged_node.c_str(), merged_renamed.c_str())) << strerror(errno);

    struct stat st = {};
    ASSERT_EQ(0, lstat(upper_renamed.c_str(), &st)) << strerror(errno);
    EXPECT_TRUE(S_ISBLK(st.st_mode));
    EXPECT_EQ(dev, st.st_rdev);
    ASSERT_EQ(0, lstat(merged_renamed.c_str(), &st)) << strerror(errno);
    EXPECT_TRUE(S_ISBLK(st.st_mode));
    EXPECT_EQ(dev, st.st_rdev);
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, CopyUpLowerFifoPreservesType) {
    ScopedOverlayEnv scoped("overlayfs_copy_up_fifo");
    const auto& env = scoped.env;
    std::string lower_fifo = join_path(env.lower, "fifo");
    std::string upper_renamed = join_path(env.upper, "renamed-fifo");
    std::string merged_fifo = join_path(env.merged, "fifo");
    std::string merged_renamed = join_path(env.merged, "renamed-fifo");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, mkfifo(lower_fifo.c_str(), 0600)) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    ASSERT_EQ(0, rename(merged_fifo.c_str(), merged_renamed.c_str())) << strerror(errno);

    struct stat st = {};
    ASSERT_EQ(0, lstat(upper_renamed.c_str(), &st)) << strerror(errno);
    EXPECT_TRUE(S_ISFIFO(st.st_mode));
    ASSERT_EQ(0, lstat(merged_renamed.c_str(), &st)) << strerror(errno);
    EXPECT_TRUE(S_ISFIFO(st.st_mode));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, CopyUpLowerSocketPreservesType) {
    ScopedOverlayEnv scoped("overlayfs_copy_up_socket");
    const auto& env = scoped.env;
    std::string lower_socket = join_path(env.lower, "sock");
    std::string upper_renamed = join_path(env.upper, "renamed-sock");
    std::string merged_socket = join_path(env.merged, "sock");
    std::string merged_renamed = join_path(env.merged, "renamed-sock");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, mknod(lower_socket.c_str(), S_IFSOCK | 0600, 0)) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    ASSERT_EQ(0, rename(merged_socket.c_str(), merged_renamed.c_str())) << strerror(errno);

    struct stat st = {};
    ASSERT_EQ(0, lstat(upper_renamed.c_str(), &st)) << strerror(errno);
    EXPECT_TRUE(S_ISSOCK(st.st_mode));
    ASSERT_EQ(0, lstat(merged_renamed.c_str(), &st)) << strerror(errno);
    EXPECT_TRUE(S_ISSOCK(st.st_mode));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, OpenLowerFifoForWriteDoesNotCopyUp) {
    ScopedOverlayEnv scoped("overlayfs_open_fifo_no_copy_up");
    const auto& env = scoped.env;
    std::string lower_fifo = join_path(env.lower, "fifo");
    std::string upper_fifo = join_path(env.upper, "fifo");
    std::string merged_fifo = join_path(env.merged, "fifo");

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(env.root.c_str()));
    ASSERT_EQ(0, ensure_dir(env.upper.c_str()));
    ASSERT_EQ(0, ensure_dir(env.lower.c_str()));
    ASSERT_EQ(0, ensure_dir(env.work.c_str()));
    ASSERT_EQ(0, ensure_dir(env.merged.c_str()));
    ASSERT_EQ(0, mkfifo(lower_fifo.c_str(), 0600)) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    int fd = open(merged_fifo.c_str(), O_WRONLY | O_NONBLOCK | O_CLOEXEC);
    ASSERT_EQ(-1, fd);
    EXPECT_EQ(ENXIO, errno);

    struct stat st = {};
    EXPECT_EQ(-1, lstat(upper_fifo.c_str(), &st));
    EXPECT_EQ(ENOENT, errno);
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

TEST(OverlayFsSemantics, RenameSamePathValidatesSourceAndNoReplace) {
    ScopedOverlayEnv scoped("overlayfs_rename_same_path");
    const auto& env = scoped.env;
    prepare_overlay_env(env);
    std::string merged_file = join_path(env.merged, "file");
    ASSERT_EQ(0, write_text(join_path(env.lower, "file"), "lower"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    EXPECT_EQ(0, rename(merged_file.c_str(), merged_file.c_str())) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, renameat2_call(merged_file, merged_file, RENAME_NOREPLACE));
    EXPECT_EQ(EEXIST, errno);

    std::string missing = join_path(env.merged, "missing");
    errno = 0;
    EXPECT_EQ(-1, rename(missing.c_str(), missing.c_str()));
    EXPECT_EQ(ENOENT, errno);
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

TEST(OverlayFsSemantics, CopiedUpLowerFileUnlinkKeepsOldNameHidden) {
    ScopedOverlayEnv scoped("overlayfs_copied_up_unlink_whiteout");
    const auto& env = scoped.env;
    std::string lower_file = join_path(env.lower, "file");
    std::string upper_file = join_path(env.upper, "file");
    std::string merged_file = join_path(env.merged, "file");

    prepare_overlay_env(env);
    ASSERT_EQ(0, write_text(lower_file, "lower-original"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);
    ASSERT_EQ(0, write_text(merged_file, "copied-up")) << strerror(errno);
    ASSERT_EQ("copied-up", read_text(upper_file));

    ASSERT_EQ(0, unlink(merged_file.c_str())) << strerror(errno);

    expect_path_enoent(merged_file);
    EXPECT_EQ("lower-original", read_text(lower_file));
    EXPECT_TRUE(is_whiteout(upper_file));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, CopiedUpLowerFileRenameKeepsOldNameHidden) {
    ScopedOverlayEnv scoped("overlayfs_copied_up_rename_whiteout");
    const auto& env = scoped.env;
    std::string lower_old = join_path(env.lower, "old");
    std::string upper_old = join_path(env.upper, "old");
    std::string upper_new = join_path(env.upper, "new");
    std::string merged_old = join_path(env.merged, "old");
    std::string merged_new = join_path(env.merged, "new");

    prepare_overlay_env(env);
    ASSERT_EQ(0, write_text(lower_old, "lower-original"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);
    ASSERT_EQ(0, write_text(merged_old, "copied-up")) << strerror(errno);

    ASSERT_EQ(0, rename(merged_old.c_str(), merged_new.c_str())) << strerror(errno);

    expect_path_enoent(merged_old);
    EXPECT_EQ("copied-up", read_text(merged_new));
    EXPECT_EQ("lower-original", read_text(lower_old));
    EXPECT_TRUE(is_whiteout(upper_old));
    EXPECT_EQ("copied-up", read_text(upper_new));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, RmdirCleansDetachedUpperChildWhiteouts) {
    ScopedOverlayEnv scoped("overlayfs_rmdir_internal_whiteouts");
    const auto& env = scoped.env;
    std::string lower_dir = join_path(env.lower, "dir");
    std::string lower_child = join_path(lower_dir, "child");
    std::string upper_dir = join_path(env.upper, "dir");
    std::string upper_child = join_path(upper_dir, "child");
    std::string merged_dir = join_path(env.merged, "dir");
    std::string merged_child = join_path(merged_dir, "child");

    prepare_overlay_env(env);
    ASSERT_EQ(0, mkdir(lower_dir.c_str(), 0755));
    ASSERT_EQ(0, write_text(lower_child, "lower-child"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);
    ASSERT_EQ(0, unlink(merged_child.c_str())) << strerror(errno);
    ASSERT_TRUE(is_whiteout(upper_child));

    ASSERT_EQ(0, rmdir(merged_dir.c_str())) << strerror(errno);

    expect_path_enoent(merged_dir);
    EXPECT_TRUE(is_whiteout(upper_dir));
    EXPECT_EQ("lower-child", read_text(lower_child));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, UpperFileOverLowerDirUnlinkLeavesWhiteout) {
    ScopedOverlayEnv scoped("overlayfs_upper_file_lower_dir_unlink");
    const auto& env = scoped.env;
    std::string upper_entry = join_path(env.upper, "entry");
    std::string lower_entry = join_path(env.lower, "entry");
    std::string merged_entry = join_path(env.merged, "entry");

    prepare_overlay_env(env);
    ASSERT_EQ(0, write_text(upper_entry, "upper-file"));
    ASSERT_EQ(0, mkdir(lower_entry.c_str(), 0755));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, rmdir(merged_entry.c_str()));
    EXPECT_EQ(ENOTDIR, errno);
    EXPECT_EQ("upper-file", read_text(merged_entry));

    ASSERT_EQ(0, unlink(merged_entry.c_str())) << strerror(errno);
    expect_path_enoent(merged_entry);
    EXPECT_TRUE(is_whiteout(upper_entry));
    EXPECT_TRUE(path_exists(lower_entry));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, UpperDirOverLowerFileRmdirLeavesWhiteout) {
    ScopedOverlayEnv scoped("overlayfs_upper_dir_lower_file_rmdir");
    const auto& env = scoped.env;
    std::string upper_entry = join_path(env.upper, "entry");
    std::string lower_entry = join_path(env.lower, "entry");
    std::string merged_entry = join_path(env.merged, "entry");

    prepare_overlay_env(env);
    ASSERT_EQ(0, mkdir(upper_entry.c_str(), 0755));
    ASSERT_EQ(0, write_text(lower_entry, "lower-file"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, unlink(merged_entry.c_str()));
    EXPECT_EQ(EISDIR, errno);
    EXPECT_TRUE(path_exists(merged_entry));

    ASSERT_EQ(0, rmdir(merged_entry.c_str())) << strerror(errno);
    expect_path_enoent(merged_entry);
    EXPECT_TRUE(is_whiteout(upper_entry));
    EXPECT_EQ("lower-file", read_text(lower_entry));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, UpperDirOverLowerFileRenameLeavesWhiteout) {
    ScopedOverlayEnv scoped("overlayfs_upper_dir_lower_file_rename");
    const auto& env = scoped.env;
    std::string upper_old = join_path(env.upper, "old");
    std::string upper_new = join_path(env.upper, "new");
    std::string lower_old = join_path(env.lower, "old");
    std::string merged_old = join_path(env.merged, "old");
    std::string merged_new = join_path(env.merged, "new");

    prepare_overlay_env(env);
    ASSERT_EQ(0, mkdir(upper_old.c_str(), 0755));
    ASSERT_EQ(0, write_text(lower_old, "lower-file"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    ASSERT_EQ(0, rename(merged_old.c_str(), merged_new.c_str())) << strerror(errno);

    expect_path_enoent(merged_old);
    EXPECT_TRUE(is_whiteout(upper_old));
    EXPECT_TRUE(path_exists(merged_new));
    EXPECT_TRUE(path_exists(upper_new));
    EXPECT_EQ("lower-file", read_text(lower_old));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, CopiedUpRenameOverTargetWhiteoutExchangesWhiteout) {
    ScopedOverlayEnv scoped("overlayfs_copied_up_rename_target_whiteout");
    const auto& env = scoped.env;
    std::string lower_old = join_path(env.lower, "old");
    std::string lower_new = join_path(env.lower, "new");
    std::string upper_old = join_path(env.upper, "old");
    std::string upper_new = join_path(env.upper, "new");
    std::string merged_old = join_path(env.merged, "old");
    std::string merged_new = join_path(env.merged, "new");

    prepare_overlay_env(env);
    ASSERT_EQ(0, write_text(lower_old, "lower-old"));
    ASSERT_EQ(0, write_text(lower_new, "lower-new"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);
    ASSERT_EQ(0, write_text(merged_old, "copied-up"));
    ASSERT_EQ(0, unlink(merged_new.c_str())) << strerror(errno);
    ASSERT_TRUE(is_whiteout(upper_new));

    ASSERT_EQ(0, renameat2_call(merged_old, merged_new, RENAME_NOREPLACE)) << strerror(errno);

    expect_path_enoent(merged_old);
    EXPECT_TRUE(is_whiteout(upper_old));
    EXPECT_EQ("copied-up", read_text(merged_new));
    EXPECT_EQ("copied-up", read_text(upper_new));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, CopiedUpCrossDirRenameScansOldLowerParent) {
    ScopedOverlayEnv scoped("overlayfs_copied_up_cross_dir_rename");
    const auto& env = scoped.env;
    std::string lower_a = join_path(env.lower, "a");
    std::string lower_b = join_path(env.lower, "b");
    std::string lower_old = join_path(lower_a, "old");
    std::string merged_a = join_path(env.merged, "a");
    std::string merged_b = join_path(env.merged, "b");
    std::string merged_old = join_path(merged_a, "old");
    std::string merged_new = join_path(merged_b, "new");
    std::string upper_old = join_path(join_path(env.upper, "a"), "old");

    prepare_overlay_env(env);
    ASSERT_EQ(0, mkdir(lower_a.c_str(), 0755));
    ASSERT_EQ(0, mkdir(lower_b.c_str(), 0755));
    ASSERT_EQ(0, write_text(lower_old, "lower-old"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);
    ASSERT_EQ(0, write_text(merged_old, "copied-up"));

    ASSERT_EQ(0, rename(merged_old.c_str(), merged_new.c_str())) << strerror(errno);

    expect_path_enoent(merged_old);
    EXPECT_TRUE(is_whiteout(upper_old));
    EXPECT_EQ("copied-up", read_text(merged_new));
    EXPECT_EQ("lower-old", read_text(lower_old));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, CopiedUpCrossDirRenameOverWhiteoutExchangesWhiteout) {
    ScopedOverlayEnv scoped("overlayfs_cross_dir_target_whiteout");
    const auto& env = scoped.env;
    std::string lower_a = join_path(env.lower, "a");
    std::string lower_b = join_path(env.lower, "b");
    std::string lower_old = join_path(lower_a, "old");
    std::string lower_new = join_path(lower_b, "new");
    std::string merged_a = join_path(env.merged, "a");
    std::string merged_b = join_path(env.merged, "b");
    std::string merged_old = join_path(merged_a, "old");
    std::string merged_new = join_path(merged_b, "new");
    std::string upper_old = join_path(join_path(env.upper, "a"), "old");
    std::string upper_new = join_path(join_path(env.upper, "b"), "new");

    prepare_overlay_env(env);
    ASSERT_EQ(0, mkdir(lower_a.c_str(), 0755));
    ASSERT_EQ(0, mkdir(lower_b.c_str(), 0755));
    ASSERT_EQ(0, write_text(lower_old, "lower-old"));
    ASSERT_EQ(0, write_text(lower_new, "lower-new"));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);
    ASSERT_EQ(0, write_text(merged_old, "copied-up"));
    ASSERT_EQ(0, unlink(merged_new.c_str())) << strerror(errno);
    ASSERT_TRUE(is_whiteout(upper_new));

    ASSERT_EQ(0, renameat2_call(merged_old, merged_new, RENAME_NOREPLACE)) << strerror(errno);

    expect_path_enoent(merged_old);
    EXPECT_TRUE(is_whiteout(upper_old));
    EXPECT_EQ("copied-up", read_text(merged_new));
    EXPECT_EQ("copied-up", read_text(upper_new));
    EXPECT_EQ("lower-old", read_text(lower_old));
    EXPECT_EQ("lower-new", read_text(lower_new));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, PureUpperUnlinkAndRmdirDoNotCreateWhiteouts) {
    ScopedOverlayEnv scoped("overlayfs_pure_upper_remove");
    const auto& env = scoped.env;
    std::string upper_file = join_path(env.upper, "file");
    std::string upper_dir = join_path(env.upper, "dir");
    std::string merged_file = join_path(env.merged, "file");
    std::string merged_dir = join_path(env.merged, "dir");

    prepare_overlay_env(env);
    ASSERT_EQ(0, write_text(upper_file, "upper-file"));
    ASSERT_EQ(0, mkdir(upper_dir.c_str(), 0755));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    ASSERT_EQ(0, unlink(merged_file.c_str())) << strerror(errno);
    ASSERT_EQ(0, rmdir(merged_dir.c_str())) << strerror(errno);

    expect_path_enoent(merged_file);
    expect_path_enoent(merged_dir);
    expect_path_enoent(upper_file);
    expect_path_enoent(upper_dir);
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
}

TEST(OverlayFsSemantics, LowerWhiteoutMakesCoveredUpperRemovalPureUpper) {
    std::string root =
        std::string("/tmp/overlayfs_lower_whiteout_remove_") + std::to_string(getpid());
    std::string upper = join_path(root, "u");
    std::string lower_top = join_path(root, "l1");
    std::string lower_bottom = join_path(root, "l2");
    std::string work = join_path(root, "w");
    std::string merged = join_path(root, "m");
    std::string upper_entry = join_path(upper, "entry");
    std::string top_entry = join_path(lower_top, "entry");
    std::string bottom_entry = join_path(lower_bottom, "entry");
    std::string merged_entry = join_path(merged, "entry");
    ScopedCustomMount cleanup(root, merged);

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(root.c_str()));
    ASSERT_EQ(0, ensure_dir(upper.c_str()));
    ASSERT_EQ(0, ensure_dir(lower_top.c_str()));
    ASSERT_EQ(0, ensure_dir(lower_bottom.c_str()));
    ASSERT_EQ(0, ensure_dir(work.c_str()));
    ASSERT_EQ(0, ensure_dir(merged.c_str()));
    ASSERT_EQ(0, write_text(upper_entry, "upper"));
    ASSERT_EQ(0, mknod(top_entry.c_str(), S_IFCHR | 0600, makedev(0, 0)));
    ASSERT_EQ(0, write_text(bottom_entry, "bottom"));
    std::string options = "lowerdir=" + lower_top + ":" + lower_bottom + ",upperdir="
        + upper + ",workdir=" + work;
    ASSERT_EQ(0, mount("overlay", merged.c_str(), "overlay", 0, options.c_str()))
        << strerror(errno);
    cleanup.mounted = true;

    ASSERT_EQ(0, unlink(merged_entry.c_str())) << strerror(errno);

    expect_path_enoent(merged_entry);
    expect_path_enoent(upper_entry);
    EXPECT_TRUE(is_whiteout(top_entry));
    EXPECT_EQ("bottom", read_text(bottom_entry));
    EXPECT_EQ(0, overlay_temp_entry_count(work));

    ASSERT_EQ(0, umount(merged.c_str())) << strerror(errno);
    cleanup.mounted = false;
}

TEST(OverlayFsSemantics, LowerPositiveContinuesPastMissingTopLayer) {
    std::string root =
        std::string("/tmp/overlayfs_lower_positive_continue_") + std::to_string(getpid());
    std::string upper = join_path(root, "u");
    std::string lower_top = join_path(root, "l1");
    std::string lower_bottom = join_path(root, "l2");
    std::string work = join_path(root, "w");
    std::string merged = join_path(root, "m");
    std::string upper_entry = join_path(upper, "entry");
    std::string bottom_entry = join_path(lower_bottom, "entry");
    std::string merged_entry = join_path(merged, "entry");
    ScopedCustomMount cleanup(root, merged);

    ASSERT_EQ(0, ensure_dir("/tmp"));
    ASSERT_EQ(0, ensure_dir(root.c_str()));
    ASSERT_EQ(0, ensure_dir(upper.c_str()));
    ASSERT_EQ(0, ensure_dir(lower_top.c_str()));
    ASSERT_EQ(0, ensure_dir(lower_bottom.c_str()));
    ASSERT_EQ(0, ensure_dir(work.c_str()));
    ASSERT_EQ(0, ensure_dir(merged.c_str()));
    ASSERT_EQ(0, write_text(upper_entry, "upper"));
    ASSERT_EQ(0, write_text(bottom_entry, "bottom"));
    std::string options = "lowerdir=" + lower_top + ":" + lower_bottom + ",upperdir="
        + upper + ",workdir=" + work;
    ASSERT_EQ(0, mount("overlay", merged.c_str(), "overlay", 0, options.c_str()))
        << strerror(errno);
    cleanup.mounted = true;

    ASSERT_EQ(0, unlink(merged_entry.c_str())) << strerror(errno);

    expect_path_enoent(merged_entry);
    EXPECT_TRUE(is_whiteout(upper_entry));
    EXPECT_EQ("bottom", read_text(bottom_entry));
    EXPECT_EQ(0, overlay_temp_entry_count(work));

    ASSERT_EQ(0, umount(merged.c_str())) << strerror(errno);
    cleanup.mounted = false;
}

TEST(OverlayFsSemantics, ConcurrentCopiedUpUnlinkNeverExposesLowerContent) {
    constexpr int kFileCount = 32;
    constexpr int kReadRounds = 128;
    ScopedOverlayEnv scoped("overlayfs_unlink_visibility");
    const auto& env = scoped.env;
    std::vector<std::string> merged_files;
    std::vector<std::string> upper_files;

    prepare_overlay_env(env);
    for (int i = 0; i < kFileCount; ++i) {
        std::string name = "file" + std::to_string(i);
        std::string lower_file = join_path(env.lower, name.c_str());
        merged_files.push_back(join_path(env.merged, name.c_str()));
        upper_files.push_back(join_path(env.upper, name.c_str()));
        ASSERT_EQ(0, write_text(lower_file, "lower-content"));
    }
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);
    for (const auto& path : merged_files) {
        ASSERT_EQ(0, write_text(path, "upper-content"));
    }

    int start_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(start_pipe)) << strerror(errno);
    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        close(start_pipe[1]);
        char token = 0;
        if (read(start_pipe[0], &token, 1) != 1) {
            _exit(2);
        }
        close(start_pipe[0]);

        for (int round = 0; round < kReadRounds; ++round) {
            for (const auto& path : merged_files) {
                int fd = open(path.c_str(), O_RDONLY | O_CLOEXEC);
                if (fd < 0) {
                    if (errno != ENOENT) {
                        _exit(3);
                    }
                    continue;
                }
                char buf[32] = {};
                ssize_t n = read(fd, buf, sizeof(buf) - 1);
                close(fd);
                if (n < 0) {
                    _exit(4);
                }
                std::string content(buf, static_cast<size_t>(n));
                if (content == "lower-content") {
                    _exit(5);
                }
                if (content != "upper-content") {
                    _exit(6);
                }
            }
            sched_yield();
        }
        _exit(0);
    }

    close(start_pipe[0]);
    ASSERT_EQ(1, write(start_pipe[1], "x", 1));
    close(start_pipe[1]);
    for (const auto& path : merged_files) {
        ASSERT_EQ(0, unlink(path.c_str())) << path << ": " << strerror(errno);
        sched_yield();
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    for (size_t i = 0; i < merged_files.size(); ++i) {
        expect_path_enoent(merged_files[i]);
        EXPECT_TRUE(is_whiteout(upper_files[i]));
    }
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
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

TEST(OverlayFsSemantics, CopyUpDoesNotAdoptConcurrentReplacement) {
    constexpr size_t kCopySize = 64 * 1024 * 1024;
    ScopedOverlayEnv scoped("overlayfs_copy_up_replace_race");
    const auto& env = scoped.env;
    std::string lower_file = join_path(env.lower, "entry");
    std::string upper_file = join_path(env.upper, "entry");
    std::string merged_file = join_path(env.merged, "entry");

    prepare_overlay_env(env);
    int lower_fd = open(lower_file.c_str(), O_CREAT | O_WRONLY | O_CLOEXEC, 0644);
    ASSERT_GE(lower_fd, 0) << strerror(errno);
    std::vector<char> block(1024 * 1024, 'l');
    for (size_t offset = 0; offset < kCopySize; offset += block.size()) {
        ASSERT_EQ(static_cast<ssize_t>(block.size()),
            write(lower_fd, block.data(), block.size()))
            << strerror(errno);
    }
    ASSERT_EQ(0, close(lower_fd)) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        int fd = open(merged_file.c_str(), O_WRONLY | O_CLOEXEC);
        if (fd < 0) {
            _exit(errno == ESTALE ? 0 : 2);
        }
        if (write(fd, "stale", 5) != 5) {
            close(fd);
            _exit(4);
        }
        close(fd);
        _exit(3);
    }

    bool copy_up_started = false;
    for (int attempt = 0; attempt < 20000; ++attempt) {
        if (overlay_temp_entry_count(env.work) > 0) {
            copy_up_started = true;
            break;
        }
        usleep(100);
    }
    ASSERT_TRUE(copy_up_started);
    ASSERT_EQ(0, write_text(upper_file, "replacement")) << strerror(errno);

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_EQ("replacement", read_text(merged_file));
    EXPECT_EQ(0, overlay_temp_entry_count(env.work));
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
    std::string upper_old_child = join_path(upper_old, "child");
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
    ASSERT_EQ(0, write_text(upper_old_child, "source"));
    ASSERT_EQ(0, mkdir(lower_new.c_str(), 0755));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    int old_target_fd = open(merged_new.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    ASSERT_GE(old_target_fd, 0) << strerror(errno);
    ASSERT_EQ(0, rename(merged_old.c_str(), merged_new.c_str())) << strerror(errno);

    EXPECT_FALSE(path_exists(merged_old));
    EXPECT_TRUE(path_exists(merged_new));
    EXPECT_FALSE(path_exists(upper_old));
    EXPECT_TRUE(path_exists(upper_new));
    EXPECT_EQ("source", read_text(join_path(merged_new, "child")));
    EXPECT_EQ(0, close(old_target_fd)) << strerror(errno);
    cleanup_overlay_env(env);
}

TEST(OverlayFsSemantics, CopiedUpIdentitySurvivesRemount) {
    if (!root_supports_ext4_xattrs()) {
        GTEST_SKIP() << "requires the Ubuntu 24.04 ext4 rootfs backing";
    }

    ScopedOverlayEnv scoped("overlayfs_remount_identity", "/root");
    const auto& env = scoped.env;
    prepare_overlay_env(env);
    std::string lower_file = join_path(env.lower, "file");
    std::string merged_file = join_path(env.merged, "file");
    ASSERT_EQ(0, write_text(lower_file, "lower")) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    struct stat before = {};
    struct stat copied_up = {};
    struct stat remounted = {};
    ASSERT_EQ(0, stat(merged_file.c_str(), &before)) << strerror(errno);
    ASSERT_EQ(0, chmod(merged_file.c_str(), 0600)) << strerror(errno);
    ASSERT_EQ(0, stat(merged_file.c_str(), &copied_up)) << strerror(errno);
    EXPECT_EQ(before.st_dev, copied_up.st_dev);
    EXPECT_EQ(before.st_ino, copied_up.st_ino);

    ASSERT_EQ(0, umount(env.merged.c_str())) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);
    ASSERT_EQ(0, stat(merged_file.c_str(), &remounted)) << strerror(errno);
    EXPECT_EQ(before.st_dev, remounted.st_dev);
    EXPECT_EQ(before.st_ino, remounted.st_ino);
}

TEST(OverlayFsSemantics, LowerMetadataMutationsCopyUpAndKeepStableIdentity) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root for chown coverage";
    }

    if (!root_supports_ext4_xattrs()) {
        GTEST_SKIP() << "requires the Ubuntu 24.04 ext4 rootfs backing";
    }

    ScopedOverlayEnv scoped("overlayfs_metadata_copy_up", "/root");
    const auto& env = scoped.env;
    prepare_overlay_env(env);

    const char* names[] = {"mode", "owner", "time", "truncate", "ftruncate"};
    for (const char* name : names) {
        ASSERT_EQ(0, write_text(join_path(env.lower, name), "0123456789")) << strerror(errno);
    }
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    struct stat before[5] = {};
    for (size_t i = 0; i < 5; ++i) {
        ASSERT_EQ(0, stat(join_path(env.merged, names[i]).c_str(), &before[i])) << strerror(errno);
    }
    sleep(1);

    ASSERT_EQ(0, chmod(join_path(env.merged, names[0]).c_str(), 0601)) << strerror(errno);
    ASSERT_EQ(0, chown(join_path(env.merged, names[1]).c_str(), 1000, 1001)) << strerror(errno);
    timespec times[2] = {{123456789, 123456789}, {123456790, 987654321}};
    ASSERT_EQ(0, utimensat(AT_FDCWD, join_path(env.merged, names[2]).c_str(), times, 0))
        << strerror(errno);
    ASSERT_EQ(0, truncate(join_path(env.merged, names[3]).c_str(), 3)) << strerror(errno);
    int fd = open(join_path(env.merged, names[4]).c_str(), O_WRONLY | O_CLOEXEC);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(0, ftruncate(fd, 4)) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);

    EXPECT_EQ("012", read_text(join_path(env.upper, names[3])));
    EXPECT_EQ("012", read_text(join_path(env.merged, names[3])));
    EXPECT_EQ("0123", read_text(join_path(env.upper, names[4])));
    EXPECT_EQ("0123", read_text(join_path(env.merged, names[4])));

    for (size_t i = 0; i < 5; ++i) {
        struct stat merged_st = {};
        struct stat lower_st = {};
        struct stat upper_st = {};
        ASSERT_EQ(0, stat(join_path(env.merged, names[i]).c_str(), &merged_st)) << strerror(errno);
        ASSERT_EQ(0, stat(join_path(env.lower, names[i]).c_str(), &lower_st)) << strerror(errno);
        ASSERT_EQ(0, stat(join_path(env.upper, names[i]).c_str(), &upper_st)) << strerror(errno);
        EXPECT_EQ(before[i].st_dev, merged_st.st_dev) << names[i];
        EXPECT_EQ(before[i].st_ino, merged_st.st_ino) << names[i];
        EXPECT_GT(merged_st.st_ctim.tv_sec, before[i].st_ctim.tv_sec) << names[i];
        EXPECT_EQ(10, lower_st.st_size) << names[i];
        if (i >= 3) {
            EXPECT_GT(merged_st.st_mtim.tv_sec, before[i].st_mtim.tv_sec) << names[i];
        }
    }

    struct stat st = {};
    ASSERT_EQ(0, stat(join_path(env.merged, names[0]).c_str(), &st));
    EXPECT_EQ(0601u, st.st_mode & 07777);
    ASSERT_EQ(0, stat(join_path(env.lower, names[0]).c_str(), &st));
    EXPECT_EQ(0644u, st.st_mode & 07777);
    ASSERT_EQ(0, stat(join_path(env.merged, names[1]).c_str(), &st));
    EXPECT_EQ(1000u, st.st_uid);
    EXPECT_EQ(1001u, st.st_gid);
    ASSERT_EQ(0, stat(join_path(env.lower, names[1]).c_str(), &st));
    EXPECT_EQ(0u, st.st_uid);
    EXPECT_EQ(0u, st.st_gid);
    ASSERT_EQ(0, stat(join_path(env.merged, names[2]).c_str(), &st));
    EXPECT_EQ(times[0].tv_sec, st.st_atim.tv_sec);
    EXPECT_EQ(times[1].tv_sec, st.st_mtim.tv_sec);
    // DragonOS ext4 currently exposes second-resolution timestamps.
    EXPECT_EQ(0, st.st_atim.tv_nsec);
    EXPECT_EQ(0, st.st_mtim.tv_nsec);
    ASSERT_EQ(0, stat(join_path(env.lower, names[2]).c_str(), &st));
    EXPECT_NE(times[1].tv_sec, st.st_mtim.tv_sec);
    ASSERT_EQ(0, stat(join_path(env.merged, names[3]).c_str(), &st));
    EXPECT_EQ(3, st.st_size);
    ASSERT_EQ(0, stat(join_path(env.merged, names[4]).c_str(), &st));
    EXPECT_EQ(4, st.st_size);

    std::string pure_upper = join_path(env.merged, "pure_upper_time");
    ASSERT_EQ(0, write_text(pure_upper, "upper"));
    struct stat pure_before = {};
    ASSERT_EQ(0, stat(pure_upper.c_str(), &pure_before));
    sleep(1);
    ASSERT_EQ(0, chmod(pure_upper.c_str(), 0600));
    struct stat pure_after_chmod = {};
    ASSERT_EQ(0, stat(pure_upper.c_str(), &pure_after_chmod));
    EXPECT_GT(pure_after_chmod.st_ctim.tv_sec, pure_before.st_ctim.tv_sec);
    sleep(1);
    ASSERT_EQ(0, truncate(pure_upper.c_str(), 2));
    struct stat pure_after_truncate = {};
    ASSERT_EQ(0, stat(pure_upper.c_str(), &pure_after_truncate));
    EXPECT_GT(pure_after_truncate.st_ctim.tv_sec, pure_after_chmod.st_ctim.tv_sec);
    EXPECT_GT(pure_after_truncate.st_mtim.tv_sec, pure_after_chmod.st_mtim.tv_sec);
}

TEST(OverlayFsSemantics, UserXattrOperationsAndPrivateNamespaceSemantics) {
    if (!root_supports_ext4_xattrs()) {
        GTEST_SKIP() << "requires the Ubuntu 24.04 ext4 rootfs backing";
    }

    ScopedOverlayEnv scoped("overlayfs_xattr_ops", "/root");
    const auto& env = scoped.env;
    prepare_overlay_env(env);
    std::string lower_file = join_path(env.lower, "file");
    std::string merged_file = join_path(env.merged, "file");
    std::string upper_file = join_path(env.upper, "file");
    std::string missing_lower = join_path(env.lower, "missing");
    std::string missing_merged = join_path(env.merged, "missing");
    std::string missing_upper = join_path(env.upper, "missing");
    ASSERT_EQ(0, write_text(lower_file, "lower"));
    ASSERT_EQ(0, write_text(missing_lower, "lower"));
    ASSERT_EQ(0, setxattr(lower_file.c_str(), "user.base", "lower", 5, 0)) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    expect_xattr(merged_file, "user.base", "lower", 5);
    EXPECT_TRUE(has_xattr_name(xattr_names(merged_file), "user.base"));
    ASSERT_EQ(0, setxattr(merged_file.c_str(), "user.base", "upper", 5, XATTR_REPLACE))
        << strerror(errno);
    expect_xattr(merged_file, "user.base", "upper", 5);
    expect_xattr(upper_file, "user.base", "upper", 5);
    expect_xattr(lower_file, "user.base", "lower", 5);
    errno = 0;
    EXPECT_EQ(-1, setxattr(merged_file.c_str(), "user.base", "new", 3, XATTR_CREATE));
    EXPECT_EQ(EEXIST, errno);
    ASSERT_EQ(0, setxattr(merged_file.c_str(), "user.created", "value", 5, XATTR_CREATE));
    ASSERT_EQ(0, removexattr(merged_file.c_str(), "user.base"));
    errno = 0;
    EXPECT_EQ(-1, getxattr(merged_file.c_str(), "user.base", nullptr, 0));
    EXPECT_EQ(ENODATA, errno);

    errno = 0;
    EXPECT_EQ(-1, removexattr(missing_merged.c_str(), "user.absent"));
    EXPECT_EQ(ENODATA, errno);
    EXPECT_FALSE(path_exists(missing_upper));

    constexpr const char* kPrivate = "trusted.overlay.origin";
    errno = 0;
    EXPECT_EQ(-1, getxattr(merged_file.c_str(), kPrivate, nullptr, 0));
    EXPECT_EQ(EOPNOTSUPP, errno);
    errno = 0;
    EXPECT_EQ(-1, setxattr(merged_file.c_str(), kPrivate, "x", 1, 0));
    EXPECT_EQ(EOPNOTSUPP, errno);
    errno = 0;
    EXPECT_EQ(-1, removexattr(merged_file.c_str(), kPrivate));
    EXPECT_EQ(EOPNOTSUPP, errno);
    errno = 0;
    EXPECT_EQ(-1,
              getxattr(merged_file.c_str(), "trusted.dragonos.overlay.origin", nullptr, 0));
    EXPECT_EQ(EOPNOTSUPP, errno);
    for (const auto& name : xattr_names(merged_file)) {
        EXPECT_NE(0u, name.find("trusted.overlay."));
        EXPECT_NE("trusted.dragonos.overlay.origin", name);
    }
}

TEST(OverlayFsSemantics, RemoveLowerXattrDroppedByUnsupportedUpperSucceeds) {
    if (!root_supports_ext4_xattrs()) {
        GTEST_SKIP() << "requires an ext4 lower and tmpfs upper";
    }

    std::string suffix = std::to_string(getpid());
    std::string lower_root = "/root/overlayfs_xattr_drop_lower_" + suffix;
    std::string upper_root = "/tmp/overlayfs_xattr_drop_upper_" + suffix;
    std::string lower = join_path(lower_root, "l");
    std::string upper = join_path(upper_root, "u");
    std::string work = join_path(upper_root, "w");
    std::string merged = join_path(upper_root, "m");
    std::string lower_file = join_path(lower, "file");
    std::string upper_file = join_path(upper, "file");
    std::string merged_file = join_path(merged, "file");
    std::string upper_probe = join_path(upper_root, "xattr_probe");
    ScopedCustomMount lower_cleanup(lower_root, "");
    ScopedCustomMount upper_cleanup(upper_root, merged);

    ASSERT_EQ(0, ensure_dir(lower_root.c_str()));
    ASSERT_EQ(0, ensure_dir(upper_root.c_str()));
    ASSERT_EQ(0, ensure_dir(lower.c_str()));
    ASSERT_EQ(0, ensure_dir(upper.c_str()));
    ASSERT_EQ(0, ensure_dir(work.c_str()));
    ASSERT_EQ(0, ensure_dir(merged.c_str()));
    ASSERT_EQ(0, write_text(lower_file, "lower"));
    ASSERT_EQ(0, write_text(upper_probe, "probe"));
    errno = 0;
    int probe_result = setxattr(upper_probe.c_str(), "user.probe", "value", 5, 0);
    if (probe_result == 0) {
        GTEST_SKIP() << "requires an upper filesystem without xattr support";
    }
    ASSERT_TRUE(is_xattr_unsupported_errno(errno)) << strerror(errno);
    ASSERT_EQ(0, setxattr(lower_file.c_str(), "user.dropped", "value", 5, 0))
        << strerror(errno);

    std::string options = "lowerdir=" + lower + ",upperdir=" + upper + ",workdir=" + work;
    ASSERT_EQ(0, mount("overlay", merged.c_str(), "overlay", 0, options.c_str()))
        << strerror(errno);
    upper_cleanup.mounted = true;

    expect_xattr(merged_file, "user.dropped", "value", 5);
    ASSERT_EQ(0, removexattr(merged_file.c_str(), "user.dropped")) << strerror(errno);
    EXPECT_TRUE(path_exists(upper_file));
    errno = 0;
    EXPECT_EQ(-1, getxattr(merged_file.c_str(), "user.dropped", nullptr, 0));
    EXPECT_TRUE(errno == ENODATA || is_xattr_unsupported_errno(errno)) << strerror(errno);
    expect_xattr(lower_file, "user.dropped", "value", 5);
}

TEST(OverlayFsSemantics, CallerPermissionsProtectTimestampsAndUserXattrs) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root to create the non-root caller";
    }
    if (!root_supports_ext4_xattrs()) {
        GTEST_SKIP() << "requires the Ubuntu 24.04 ext4 rootfs backing";
    }

    ASSERT_EQ(0, ensure_dir("/var/tmp"));
    ASSERT_EQ(0, chmod("/var/tmp", 01777));
    ScopedOverlayEnv scoped("overlayfs_caller_permissions", "/var/tmp");
    const auto& env = scoped.env;
    prepare_overlay_env(env);
    std::string lower_file = join_path(env.lower, "secret");
    std::string lower_sticky = join_path(env.lower, "sticky");
    std::string lower_fifo = join_path(env.lower, "fifo");
    std::string merged_file = join_path(env.merged, "secret");
    std::string merged_sticky = join_path(env.merged, "sticky");
    std::string merged_fifo = join_path(env.merged, "fifo");
    ASSERT_EQ(0, write_text(lower_file, "secret"));
    ASSERT_EQ(0, setxattr(lower_file.c_str(), "user.secret", "hidden", 6, 0));
    ASSERT_EQ(0, chmod(lower_file.c_str(), 0000));
    ASSERT_EQ(0, mkdir(lower_sticky.c_str(), 01777));
    ASSERT_EQ(0, chmod(lower_sticky.c_str(), 01777));
    ASSERT_EQ(0, mkfifo(lower_fifo.c_str(), 0666));
    ASSERT_EQ(0, chmod(lower_fifo.c_str(), 0666));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (setgid(1000) != 0 || setuid(1000) != 0) {
            _exit(10);
        }
        __user_cap_header_struct cap_header = {_LINUX_CAPABILITY_VERSION_3, 0};
        __user_cap_data_struct cap_data[2] = {};
        if (syscall(SYS_capset, &cap_header, cap_data) != 0) {
            _exit(16);
        }
        char value[8] = {};
        if (getxattr(merged_file.c_str(), "user.secret", value, sizeof(value)) != -1
            || errno != EACCES) {
            _exit(11);
        }
        if (setxattr(merged_sticky.c_str(), "user.denied", "x", 1, 0) != -1
            || errno != EPERM) {
            _exit(12);
        }
        if (setxattr(merged_fifo.c_str(), "user.denied", "x", 1, 0) != -1
            || errno != EPERM) {
            _exit(13);
        }
        timespec explicit_times[2] = {{123456789, 0}, {123456790, 0}};
        if (utimensat(AT_FDCWD, merged_file.c_str(), explicit_times, 0) != -1
            || errno != EPERM) {
            _exit(14);
        }
        if (utimensat(AT_FDCWD, merged_file.c_str(), nullptr, 0) != -1
            || errno != EACCES) {
            _exit(15);
        }
        _exit(0);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_FALSE(path_exists(join_path(env.upper, "secret")));
    EXPECT_FALSE(path_exists(join_path(env.upper, "sticky")));
    EXPECT_FALSE(path_exists(join_path(env.upper, "fifo")));
}

TEST(OverlayFsSemantics, CopyUpPreservesRawAclCapabilityAndAncestorMetadata) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root for ownership and security xattr coverage";
    }

    if (!root_supports_ext4_xattrs()) {
        GTEST_SKIP() << "requires the Ubuntu 24.04 ext4 rootfs backing";
    }

    ScopedOverlayEnv scoped("overlayfs_copy_up_raw_metadata", "/root");
    const auto& env = scoped.env;
    prepare_overlay_env(env);
    std::string lower_a = join_path(env.lower, "a");
    std::string lower_b = join_path(lower_a, "b");
    std::string lower_file = join_path(lower_b, "file");
    std::string upper_a = join_path(env.upper, "a");
    std::string upper_b = join_path(upper_a, "b");
    std::string upper_file = join_path(upper_b, "file");
    std::string merged_file = join_path(join_path(join_path(env.merged, "a"), "b"), "file");
    ASSERT_EQ(0, mkdir(lower_a.c_str(), 0751));
    ASSERT_EQ(0, mkdir(lower_b.c_str(), 0710));
    ASSERT_EQ(0, write_text(lower_file, "metadata"));
    ASSERT_EQ(0, chown(lower_a.c_str(), 1000, 1001));
    ASSERT_EQ(0, chown(lower_b.c_str(), 1002, 1003));
    timespec a_times[2] = {{123400000, 111}, {123400001, 222}};
    timespec b_times[2] = {{123500000, 333}, {123500001, 444}};
    ASSERT_EQ(0, utimensat(AT_FDCWD, lower_a.c_str(), a_times, 0));
    ASSERT_EQ(0, utimensat(AT_FDCWD, lower_b.c_str(), b_times, 0));

    const unsigned char acl[] = {
        2, 0, 0, 0, 1, 0, 7, 0, 0xff, 0xff, 0xff, 0xff,
        4, 0, 5, 0, 0xff, 0xff, 0xff, 0xff, 0x20, 0, 5, 0, 0xff, 0xff, 0xff, 0xff};
    const unsigned char capability[] = {
        0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0};
    ASSERT_EQ(0, setxattr(lower_a.c_str(), "user.ancestor", "a", 1, 0)) << strerror(errno);
    ASSERT_EQ(0, setxattr(lower_b.c_str(), "system.posix_acl_default", acl, sizeof(acl), 0))
        << "ext4 must accept the raw default ACL used by this test: " << strerror(errno);
    ASSERT_EQ(0, setxattr(lower_file.c_str(), "system.posix_acl_access", acl, sizeof(acl), 0))
        << "ext4 must accept the raw access ACL used by this test: " << strerror(errno);
    ASSERT_EQ(0,
              setxattr(lower_file.c_str(), "security.capability", capability, sizeof(capability), 0))
        << "ext4 must accept the raw file capability used by this test: " << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    int fd = open(merged_file.c_str(), O_WRONLY | O_CLOEXEC);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(0, close(fd));

    struct stat st = {};
    ASSERT_EQ(0, stat(upper_a.c_str(), &st));
    EXPECT_EQ(1000u, st.st_uid);
    EXPECT_EQ(1001u, st.st_gid);
    EXPECT_EQ(0751u, st.st_mode & 07777);
    EXPECT_EQ(a_times[0].tv_sec, st.st_atim.tv_sec);
    EXPECT_EQ(a_times[1].tv_sec, st.st_mtim.tv_sec);
    ASSERT_EQ(0, stat(upper_b.c_str(), &st));
    EXPECT_EQ(1002u, st.st_uid);
    EXPECT_EQ(1003u, st.st_gid);
    EXPECT_EQ(0710u, st.st_mode & 07777);
    EXPECT_EQ(b_times[0].tv_sec, st.st_atim.tv_sec);
    EXPECT_EQ(b_times[1].tv_sec, st.st_mtim.tv_sec);
    expect_xattr(upper_a, "user.ancestor", "a", 1);
    expect_xattr(upper_b, "system.posix_acl_default", acl, sizeof(acl));
    expect_xattr(upper_file, "system.posix_acl_access", acl, sizeof(acl));
    expect_xattr(upper_file, "security.capability", capability, sizeof(capability));
    expect_xattr(merged_file, "system.posix_acl_access", acl, sizeof(acl));
    expect_xattr(merged_file, "security.capability", capability, sizeof(capability));
}

TEST(OverlayFsSemantics, ContentMutationRemovesCopiedUpFileCapability) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root for security xattr coverage";
    }
    if (!root_supports_ext4_xattrs()) {
        GTEST_SKIP() << "requires the Ubuntu 24.04 ext4 rootfs backing";
    }

    ScopedOverlayEnv scoped("overlayfs_kill_file_capability", "/root");
    const auto& env = scoped.env;
    prepare_overlay_env(env);
    const unsigned char capability[] = {
        0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0};
    const char* names[] = {"write", "truncate", "chown"};
    for (const char* name : names) {
        std::string lower_file = join_path(env.lower, name);
        ASSERT_EQ(0, write_text(lower_file, "lower")) << strerror(errno);
        ASSERT_EQ(0,
                  setxattr(lower_file.c_str(), "security.capability", capability,
                           sizeof(capability), 0))
            << strerror(errno);
    }
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    std::string merged_write = join_path(env.merged, names[0]);
    int fd = open(merged_write.c_str(), O_WRONLY | O_CLOEXEC);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(1, write(fd, "x", 1)) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_EQ(0, truncate(join_path(env.merged, names[1]).c_str(), 3)) << strerror(errno);
    ASSERT_EQ(0, chown(join_path(env.merged, names[2]).c_str(), 1000, 1001)) << strerror(errno);

    for (const char* name : names) {
        std::string lower_file = join_path(env.lower, name);
        std::string upper_file = join_path(env.upper, name);
        std::string merged_file = join_path(env.merged, name);
        expect_xattr(lower_file, "security.capability", capability, sizeof(capability));
        for (const std::string* file : {&upper_file, &merged_file}) {
            errno = 0;
            EXPECT_EQ(-1, getxattr(file->c_str(), "security.capability", nullptr, 0));
            EXPECT_EQ(ENODATA, errno) << *file;
        }
    }
    EXPECT_EQ("low", read_text(join_path(env.upper, names[1])));
    EXPECT_EQ("low", read_text(join_path(env.merged, names[1])));
}

TEST(OverlayFsSemantics, NonRootOwnerCanTriggerCopyUpThroughMounterCredentials) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root to create the non-root caller";
    }

    ScopedOverlayEnv scoped("overlayfs_nonroot_copy_up");
    const auto& env = scoped.env;
    prepare_overlay_env(env);
    std::string lower_file = join_path(env.lower, "file");
    std::string merged_file = join_path(env.merged, "file");
    std::string upper_file = join_path(env.upper, "file");
    ASSERT_EQ(0, write_text(lower_file, "owned"));
    ASSERT_EQ(0, chown(lower_file.c_str(), 1000, 1000));
    ASSERT_EQ(0, chmod(lower_file.c_str(), 0644));
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (setgid(1000) != 0 || setuid(1000) != 0) {
            _exit(2);
        }
        _exit(chmod(merged_file.c_str(), 0600) == 0 ? 0 : 3);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));
    struct stat st = {};
    ASSERT_EQ(0, stat(upper_file.c_str(), &st));
    EXPECT_EQ(1000u, st.st_uid);
    EXPECT_EQ(0600u, st.st_mode & 07777);
}

TEST(OverlayFsSemantics, BindSharesDeviceAndIndependentOverlayGetsNewDevice) {
    ScopedOverlayEnv first("overlayfs_stat_device_first");
    ScopedOverlayEnv second("overlayfs_stat_device_second");
    prepare_overlay_env(first.env);
    prepare_overlay_env(second.env);
    ASSERT_EQ(0, write_text(join_path(first.env.lower, "file"), "first"));
    ASSERT_EQ(0, write_text(join_path(second.env.lower, "file"), "second"));
    ASSERT_TRUE(setup_overlay_env(first.env)) << strerror(errno);
    ASSERT_TRUE(setup_overlay_env(second.env)) << strerror(errno);

    std::string bind_path = join_path(first.env.root, "bind");
    ASSERT_EQ(0, mkdir(bind_path.c_str(), 0755));
    ASSERT_EQ(0, mount(first.env.merged.c_str(), bind_path.c_str(), nullptr, MS_BIND, nullptr))
        << strerror(errno);
    struct stat original = {};
    struct stat bound = {};
    struct stat independent = {};
    ASSERT_EQ(0, stat(join_path(first.env.merged, "file").c_str(), &original));
    ASSERT_EQ(0, stat(join_path(bind_path, "file").c_str(), &bound));
    ASSERT_EQ(0, stat(join_path(second.env.merged, "file").c_str(), &independent));
    EXPECT_EQ(original.st_dev, bound.st_dev);
    EXPECT_EQ(original.st_ino, bound.st_ino);
    EXPECT_NE(original.st_dev, independent.st_dev);
    ASSERT_EQ(0, umount(bind_path.c_str()));
    ASSERT_EQ(0, rmdir(bind_path.c_str()));
}

TEST(OverlayFsSemantics, LargeMergedDirectoryCacheInvalidatesAfterMutations) {
    ScopedOverlayEnv scoped("overlayfs_large_readdir_cache");
    const auto& env = scoped.env;
    prepare_overlay_env(env);

    constexpr int kEntryCount = 256;
    for (int i = 0; i < kEntryCount; ++i) {
        char name[32] = {};
        snprintf(name, sizeof(name), "entry_%03d", i);
        ASSERT_EQ(0, write_text(join_path(env.lower, name), "lower"));
        if ((i % 2) == 0) {
            ASSERT_EQ(0, write_text(join_path(env.upper, name), "upper"));
        }
    }
    ASSERT_TRUE(setup_overlay_env(env)) << strerror(errno);

    auto first = directory_names(env.merged);
    ASSERT_EQ(static_cast<size_t>(kEntryCount), first.size());
    EXPECT_EQ(first, directory_names(env.merged));

    std::string created = join_path(env.merged, "created");
    ASSERT_EQ(0, write_text(created, "new")) << strerror(errno);
    auto after_create = directory_names(env.merged);
    EXPECT_TRUE(std::binary_search(after_create.begin(), after_create.end(), "created"));

    ASSERT_EQ(0, unlink(join_path(env.merged, "entry_003").c_str())) << strerror(errno);
    auto after_unlink = directory_names(env.merged);
    EXPECT_FALSE(std::binary_search(after_unlink.begin(), after_unlink.end(), "entry_003"));

    std::string renamed = join_path(env.merged, "renamed");
    ASSERT_EQ(0, rename(created.c_str(), renamed.c_str())) << strerror(errno);
    auto after_rename = directory_names(env.merged);
    EXPECT_FALSE(std::binary_search(after_rename.begin(), after_rename.end(), "created"));
    EXPECT_TRUE(std::binary_search(after_rename.begin(), after_rename.end(), "renamed"));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
