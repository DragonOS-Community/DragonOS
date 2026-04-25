/**
 * rename/move 系统调用全面测试程序
 *
 * 测试 POSIX rename 语义的各种边界情况
 *
 * 在DragonOS中测试ext4时记得先umount掉tmpfs
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <dirent.h>
#include <linux/fs.h>
#include <sys/syscall.h>

/* musl libc 可能没有 renameat2 的包装函数，直接使用 syscall */
#ifndef SYS_renameat2
#error "renameat2 syscall not supported on this platform"
#endif

static inline int my_renameat2(int olddirfd, const char *oldpath,
                               int newdirfd, const char *newpath, unsigned int flags)
{
    return syscall(SYS_renameat2, olddirfd, oldpath, newdirfd, newpath, flags);
}

/* 为了兼容性，定义 renameat2 宏 */
#define renameat2 my_renameat2

#define TEST_DIR "/tmp/rename_test"

static int tests_run = 0;
static int tests_passed = 0;
static int tests_failed = 0;

#define COLOR_RED     "\x1b[31m"
#define COLOR_GREEN   "\x1b[32m"
#define COLOR_YELLOW  "\x1b[33m"
#define COLOR_RESET   "\x1b[0m"

#define TEST_BEGIN(name) \
    do { \
        tests_run++; \
        printf("  [%3d] %-50s ", tests_run, name); \
        fflush(stdout); \
    } while(0)

#define TEST_PASS() \
    do { \
        tests_passed++; \
        printf(COLOR_GREEN "PASS" COLOR_RESET "\n"); \
    } while(0)

#define TEST_FAIL(fmt, ...) \
    do { \
        tests_failed++; \
        printf(COLOR_RED "FAIL" COLOR_RESET " - " fmt "\n", ##__VA_ARGS__); \
    } while(0)

#define TEST_SKIP(reason) \
    do { \
        printf(COLOR_YELLOW "SKIP" COLOR_RESET " - %s\n", reason); \
    } while(0)

#define EXPECT_EQ(expected, actual) \
    do { \
        int _e = (expected), _a = (actual); \
        if (_e != _a) { \
            TEST_FAIL("expected %d, got %d (errno=%d: %s)", _e, _a, errno, strerror(errno)); \
            return; \
        } \
    } while(0)

#define EXPECT_ERRNO(expected_errno) \
    do { \
        if (errno != (expected_errno)) { \
            TEST_FAIL("expected errno=%d (%s), got errno=%d (%s)", \
                (expected_errno), strerror(expected_errno), errno, strerror(errno)); \
            return; \
        } \
    } while(0)

/* 辅助函数 */

static void create_file(const char *path, const char *content) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) {
        perror("create_file: open");
        exit(1);
    }
    if (content) {
        ssize_t n = write(fd, content, strlen(content));
        (void)n;  /* ignore */
    }
    close(fd);
}

static void create_dir(const char *path) {
    if (mkdir(path, 0755) < 0 && errno != EEXIST) {
        perror("create_dir: mkdir");
        exit(1);
    }
}

static int file_exists(const char *path) {
    struct stat st;
    return stat(path, &st) == 0;
}

static int is_dir(const char *path) {
    struct stat st;
    if (stat(path, &st) < 0) return 0;
    return S_ISDIR(st.st_mode);
}

static int read_file_content(const char *path, char *buf, size_t size) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;

    ssize_t n = read(fd, buf, size - 1);
    close(fd);

    if (n < 0) return -1;
    buf[n] = '\0';
    return 0;
}

static ino_t get_inode(const char *path) {
    struct stat st;
    if (stat(path, &st) < 0) return 0;
    return st.st_ino;
}

static void cleanup_dir(const char *path);

static void remove_recursive(const char *path) {
    struct stat st;
    if (lstat(path, &st) < 0) return;

    if (S_ISDIR(st.st_mode)) {
        cleanup_dir(path);
        rmdir(path);
    } else {
        unlink(path);
    }
}

static void cleanup_dir(const char *path) {
    DIR *d = opendir(path);
    if (!d) return;

    struct dirent *ent;
    char fullpath[512];

    while ((ent = readdir(d)) != NULL) {
        if (strcmp(ent->d_name, ".") == 0 || strcmp(ent->d_name, "..") == 0)
            continue;
        snprintf(fullpath, sizeof(fullpath), "%s/%s", path, ent->d_name);
        remove_recursive(fullpath);
    }
    closedir(d);
}

static void setup_test_env(void) {
    remove_recursive(TEST_DIR);
    create_dir(TEST_DIR);
}

/* ========== 测试用例 ========== */

/* 1. 基本文件重命名 */
static void test_basic_file_rename(void) {
    TEST_BEGIN("basic file rename");

    create_file(TEST_DIR "/file1.txt", "hello");

    int ret = rename(TEST_DIR "/file1.txt", TEST_DIR "/file2.txt");
    EXPECT_EQ(0, ret);

    if (!file_exists(TEST_DIR "/file2.txt")) {
        TEST_FAIL("new file does not exist");
        return;
    }
    if (file_exists(TEST_DIR "/file1.txt")) {
        TEST_FAIL("old file still exists");
        return;
    }

    char content[256];
    if (read_file_content(TEST_DIR "/file2.txt", content, sizeof(content)) < 0 ||
        strcmp(content, "hello") != 0) {
        TEST_FAIL("content mismatch");
        return;
    }

    TEST_PASS();
}

/* 2. 基本目录重命名 */
static void test_basic_dir_rename(void) {
    TEST_BEGIN("basic directory rename");

    create_dir(TEST_DIR "/dir1");
    create_file(TEST_DIR "/dir1/file.txt", "test");

    int ret = rename(TEST_DIR "/dir1", TEST_DIR "/dir2");
    EXPECT_EQ(0, ret);

    if (!is_dir(TEST_DIR "/dir2")) {
        TEST_FAIL("new dir does not exist");
        return;
    }
    if (file_exists(TEST_DIR "/dir1")) {
        TEST_FAIL("old dir still exists");
        return;
    }
    if (!file_exists(TEST_DIR "/dir2/file.txt")) {
        TEST_FAIL("file in dir missing");
        return;
    }

    TEST_PASS();
}

/* 3. 跨目录移动文件 */
static void test_cross_dir_move_file(void) {
    TEST_BEGIN("cross directory move file");

    create_dir(TEST_DIR "/src");
    create_dir(TEST_DIR "/dst");
    create_file(TEST_DIR "/src/file.txt", "data");

    int ret = rename(TEST_DIR "/src/file.txt", TEST_DIR "/dst/file.txt");
    EXPECT_EQ(0, ret);

    if (!file_exists(TEST_DIR "/dst/file.txt")) {
        TEST_FAIL("file not in destination");
        return;
    }
    if (file_exists(TEST_DIR "/src/file.txt")) {
        TEST_FAIL("file still in source");
        return;
    }

    TEST_PASS();
}

/* 4. 覆盖已存在的文件 */
static void test_overwrite_existing_file(void) {
    TEST_BEGIN("overwrite existing file");

    create_file(TEST_DIR "/old.txt", "old content");
    create_file(TEST_DIR "/new.txt", "new content");

    ino_t old_inode = get_inode(TEST_DIR "/old.txt");

    int ret = rename(TEST_DIR "/old.txt", TEST_DIR "/new.txt");
    EXPECT_EQ(0, ret);

    if (file_exists(TEST_DIR "/old.txt")) {
        TEST_FAIL("source still exists");
        return;
    }

    char content[256];
    if (read_file_content(TEST_DIR "/new.txt", content, sizeof(content)) < 0 ||
        strcmp(content, "old content") != 0) {
        TEST_FAIL("content should be from source file");
        return;
    }

    ino_t new_inode = get_inode(TEST_DIR "/new.txt");
    if (new_inode != old_inode) {
        TEST_FAIL("inode should be preserved from source");
        return;
    }

    TEST_PASS();
}

/* 5. 覆盖空目录 */
static void test_overwrite_empty_dir(void) {
    TEST_BEGIN("overwrite empty directory");

    create_dir(TEST_DIR "/src_dir");
    create_file(TEST_DIR "/src_dir/file.txt", "test");
    create_dir(TEST_DIR "/dst_dir");  /* empty */

    int ret = rename(TEST_DIR "/src_dir", TEST_DIR "/dst_dir");
    EXPECT_EQ(0, ret);

    if (file_exists(TEST_DIR "/src_dir")) {
        TEST_FAIL("source still exists");
        return;
    }
    if (!file_exists(TEST_DIR "/dst_dir/file.txt")) {
        TEST_FAIL("contents not preserved");
        return;
    }

    TEST_PASS();
}

/* 6. 不能覆盖非空目录 */
static void test_cannot_overwrite_nonempty_dir(void) {
    TEST_BEGIN("cannot overwrite non-empty directory");

    create_dir(TEST_DIR "/src_dir");
    create_dir(TEST_DIR "/dst_dir");
    create_file(TEST_DIR "/dst_dir/existing.txt", "data");

    int ret = rename(TEST_DIR "/src_dir", TEST_DIR "/dst_dir");

    if (ret == 0) {
        TEST_FAIL("should have failed");
        return;
    }
    EXPECT_ERRNO(ENOTEMPTY);

    TEST_PASS();
}

/* 7. 源文件不存在 */
static void test_source_not_exist(void) {
    TEST_BEGIN("source does not exist");

    int ret = rename(TEST_DIR "/nonexistent", TEST_DIR "/target");

    if (ret == 0) {
        TEST_FAIL("should have failed");
        return;
    }
    EXPECT_ERRNO(ENOENT);

    TEST_PASS();
}

/* 8. 目标目录不存在 */
static void test_target_dir_not_exist(void) {
    TEST_BEGIN("target directory does not exist");

    create_file(TEST_DIR "/file.txt", "data");

    int ret = rename(TEST_DIR "/file.txt", TEST_DIR "/nonexistent_dir/file.txt");

    if (ret == 0) {
        TEST_FAIL("should have failed");
        return;
    }
    EXPECT_ERRNO(ENOENT);

    TEST_PASS();
}

/* 9. 文件不能覆盖目录 */
static void test_file_cannot_overwrite_dir(void) {
    TEST_BEGIN("file cannot overwrite directory");

    create_file(TEST_DIR "/file.txt", "data");
    create_dir(TEST_DIR "/dir");

    int ret = rename(TEST_DIR "/file.txt", TEST_DIR "/dir");

    if (ret == 0) {
        TEST_FAIL("should have failed");
        return;
    }
    EXPECT_ERRNO(EISDIR);

    TEST_PASS();
}

/* 10. 目录不能覆盖文件 */
static void test_dir_cannot_overwrite_file(void) {
    TEST_BEGIN("directory cannot overwrite file");

    create_dir(TEST_DIR "/dir");
    create_file(TEST_DIR "/file.txt", "data");

    int ret = rename(TEST_DIR "/dir", TEST_DIR "/file.txt");

    if (ret == 0) {
        TEST_FAIL("should have failed");
        return;
    }
    EXPECT_ERRNO(ENOTDIR);

    TEST_PASS();
}

/* 11. 循环检测：目录不能移到自己的子目录 */
static void test_circular_rename(void) {
    TEST_BEGIN("circular rename (dir to own subdir)");

    create_dir(TEST_DIR "/parent");
    create_dir(TEST_DIR "/parent/child");
    create_dir(TEST_DIR "/parent/child/grandchild");

    int ret = rename(TEST_DIR "/parent", TEST_DIR "/parent/child/grandchild/parent");

    if (ret == 0) {
        TEST_FAIL("should have failed");
        return;
    }
    EXPECT_ERRNO(EINVAL);

    TEST_PASS();
}

/* 12. 同名重命名（无操作） */
static void test_rename_same_name(void) {
    TEST_BEGIN("rename to same name (no-op)");

    create_file(TEST_DIR "/file.txt", "data");
    ino_t inode_before = get_inode(TEST_DIR "/file.txt");

    int ret = rename(TEST_DIR "/file.txt", TEST_DIR "/file.txt");
    EXPECT_EQ(0, ret);

    ino_t inode_after = get_inode(TEST_DIR "/file.txt");
    if (inode_before != inode_after) {
        TEST_FAIL("inode changed");
        return;
    }

    TEST_PASS();
}

/* 13. 硬链接：源和目标是同一个 inode（跨目录） */
static void test_hardlink_same_inode_cross_dir(void) {
    TEST_BEGIN("hardlink same inode cross directory");

    create_dir(TEST_DIR "/dir1");
    create_dir(TEST_DIR "/dir2");
    create_file(TEST_DIR "/dir1/file.txt", "data");

    if (link(TEST_DIR "/dir1/file.txt", TEST_DIR "/dir2/file.txt") < 0) {
        TEST_SKIP("link() failed");
        return;
    }

    ino_t inode = get_inode(TEST_DIR "/dir1/file.txt");
    struct stat st_before;
    stat(TEST_DIR "/dir1/file.txt", &st_before);
    nlink_t nlink_before = st_before.st_nlink;  /* should be 2 */

    /* POSIX: 如果 old 和 new 引用同一个文件，rename 是无操作 */
    int ret = rename(TEST_DIR "/dir1/file.txt", TEST_DIR "/dir2/file.txt");
    EXPECT_EQ(0, ret);

    /* 两个名字都应该保留（无操作） */
    if (!file_exists(TEST_DIR "/dir1/file.txt")) {
        TEST_FAIL("source name should remain (no-op)");
        return;
    }
    if (!file_exists(TEST_DIR "/dir2/file.txt")) {
        TEST_FAIL("target should remain");
        return;
    }
    /* inode 不变 */
    if (get_inode(TEST_DIR "/dir2/file.txt") != inode) {
        TEST_FAIL("inode should be preserved");
        return;
    }
    /* link count 不变 */
    struct stat st_after;
    stat(TEST_DIR "/dir2/file.txt", &st_after);
    if (st_after.st_nlink != nlink_before) {
        TEST_FAIL("nlink should not change (was %lu, now %lu)",
                  (unsigned long)nlink_before, (unsigned long)st_after.st_nlink);
        return;
    }

    TEST_PASS();
}

/* 14. 硬链接：同目录下同 inode 不同名 */
static void test_hardlink_same_inode_same_dir(void) {
    TEST_BEGIN("hardlink same inode same directory");

    create_file(TEST_DIR "/file1.txt", "data");

    if (link(TEST_DIR "/file1.txt", TEST_DIR "/file2.txt") < 0) {
        TEST_SKIP("link() failed");
        return;
    }

    ino_t inode = get_inode(TEST_DIR "/file1.txt");
    struct stat st_before;
    stat(TEST_DIR "/file1.txt", &st_before);
    nlink_t nlink_before = st_before.st_nlink;  /* should be 2 */

    /* POSIX: 如果 old 和 new 引用同一个文件，rename 是无操作 */
    int ret = rename(TEST_DIR "/file1.txt", TEST_DIR "/file2.txt");
    EXPECT_EQ(0, ret);

    /* 两个名字都应该保留（无操作） */
    if (!file_exists(TEST_DIR "/file1.txt")) {
        TEST_FAIL("source name should remain (no-op)");
        return;
    }
    if (!file_exists(TEST_DIR "/file2.txt")) {
        TEST_FAIL("target name should remain");
        return;
    }
    /* inode 不变 */
    if (get_inode(TEST_DIR "/file2.txt") != inode) {
        TEST_FAIL("inode should be preserved");
        return;
    }
    /* link count 不变 */
    struct stat st_after;
    stat(TEST_DIR "/file2.txt", &st_after);
    if (st_after.st_nlink != nlink_before) {
        TEST_FAIL("nlink should not change (was %lu, now %lu)",
                  (unsigned long)nlink_before, (unsigned long)st_after.st_nlink);
        return;
    }

    TEST_PASS();
}

/* 15. 符号链接重命名 */
static void test_symlink_rename(void) {
    TEST_BEGIN("symlink rename");

    create_file(TEST_DIR "/target.txt", "data");

    if (symlink(TEST_DIR "/target.txt", TEST_DIR "/link1") < 0) {
        TEST_SKIP("symlink() failed");
        return;
    }

    int ret = rename(TEST_DIR "/link1", TEST_DIR "/link2");
    EXPECT_EQ(0, ret);

    if (file_exists(TEST_DIR "/link1")) {
        TEST_FAIL("old symlink still exists");
        return;
    }

    struct stat st;
    if (lstat(TEST_DIR "/link2", &st) < 0 || !S_ISLNK(st.st_mode)) {
        TEST_FAIL("new path is not a symlink");
        return;
    }

    TEST_PASS();
}

/* 16. rename 不跟随符号链接 */
static void test_rename_does_not_follow_symlink(void) {
    TEST_BEGIN("rename does not follow symlink");

    create_dir(TEST_DIR "/real_dir");
    create_file(TEST_DIR "/real_dir/file.txt", "data");

    if (symlink(TEST_DIR "/real_dir", TEST_DIR "/symlink") < 0) {
        TEST_SKIP("symlink() failed");
        return;
    }

    /* 重命名符号链接本身，不是它指向的目录 */
    int ret = rename(TEST_DIR "/symlink", TEST_DIR "/symlink2");
    EXPECT_EQ(0, ret);

    /* 原始目录应该还在 */
    if (!file_exists(TEST_DIR "/real_dir/file.txt")) {
        TEST_FAIL("original dir should remain");
        return;
    }

    /* 新符号链接应该存在且仍然是链接 */
    struct stat st;
    if (lstat(TEST_DIR "/symlink2", &st) < 0 || !S_ISLNK(st.st_mode)) {
        TEST_FAIL("renamed path should be symlink");
        return;
    }

    TEST_PASS();
}

/* 17. renameat2 RENAME_NOREPLACE */
static void test_rename_noreplace(void) {
    TEST_BEGIN("renameat2 RENAME_NOREPLACE");

    create_file(TEST_DIR "/src.txt", "source");
    create_file(TEST_DIR "/dst.txt", "dest");

    int ret = renameat2(AT_FDCWD, TEST_DIR "/src.txt",
                        AT_FDCWD, TEST_DIR "/dst.txt", RENAME_NOREPLACE);

    if (ret == 0) {
        TEST_FAIL("should have failed with EEXIST");
        return;
    }
    EXPECT_ERRNO(EEXIST);

    /* 两个文件都应该保持不变 */
    char src_content[256], dst_content[256];

    if (read_file_content(TEST_DIR "/src.txt", src_content, sizeof(src_content)) < 0 ||
        strcmp(src_content, "source") != 0) {
        TEST_FAIL("source file changed");
        return;
    }
    if (read_file_content(TEST_DIR "/dst.txt", dst_content, sizeof(dst_content)) < 0 ||
        strcmp(dst_content, "dest") != 0) {
        TEST_FAIL("dest file changed");
        return;
    }

    TEST_PASS();
}

/* 18. renameat2 RENAME_NOREPLACE 目标不存在时成功 */
static void test_rename_noreplace_no_target(void) {
    TEST_BEGIN("renameat2 RENAME_NOREPLACE (no target)");

    create_file(TEST_DIR "/src.txt", "data");

    int ret = renameat2(AT_FDCWD, TEST_DIR "/src.txt",
                        AT_FDCWD, TEST_DIR "/new.txt", RENAME_NOREPLACE);
    EXPECT_EQ(0, ret);

    if (file_exists(TEST_DIR "/src.txt")) {
        TEST_FAIL("source should be removed");
        return;
    }
    if (!file_exists(TEST_DIR "/new.txt")) {
        TEST_FAIL("target should exist");
        return;
    }

    TEST_PASS();
}

/* 19. renameat2 RENAME_EXCHANGE */
static void test_rename_exchange(void) {
    TEST_BEGIN("renameat2 RENAME_EXCHANGE");

    create_file(TEST_DIR "/file1.txt", "content1");
    create_file(TEST_DIR "/file2.txt", "content2");

    ino_t inode1 = get_inode(TEST_DIR "/file1.txt");
    ino_t inode2 = get_inode(TEST_DIR "/file2.txt");

    int ret = renameat2(AT_FDCWD, TEST_DIR "/file1.txt",
                        AT_FDCWD, TEST_DIR "/file2.txt", RENAME_EXCHANGE);
    EXPECT_EQ(0, ret);

    /* inode 应该交换 */
    if (get_inode(TEST_DIR "/file1.txt") != inode2) {
        TEST_FAIL("file1 should have file2's inode");
        return;
    }
    if (get_inode(TEST_DIR "/file2.txt") != inode1) {
        TEST_FAIL("file2 should have file1's inode");
        return;
    }

    /* 内容也应该交换 */
    char c1[256], c2[256];

    if (read_file_content(TEST_DIR "/file1.txt", c1, sizeof(c1)) < 0 ||
        strcmp(c1, "content2") != 0) {
        TEST_FAIL("file1 content wrong");
        return;
    }
    if (read_file_content(TEST_DIR "/file2.txt", c2, sizeof(c2)) < 0 ||
        strcmp(c2, "content1") != 0) {
        TEST_FAIL("file2 content wrong");
        return;
    }

    TEST_PASS();
}

/* 20. RENAME_EXCHANGE 目标不存在应该失败 */
static void test_rename_exchange_no_target(void) {
    TEST_BEGIN("renameat2 RENAME_EXCHANGE (no target)");

    create_file(TEST_DIR "/file.txt", "data");

    int ret = renameat2(AT_FDCWD, TEST_DIR "/file.txt",
                        AT_FDCWD, TEST_DIR "/nonexistent.txt", RENAME_EXCHANGE);

    if (ret == 0) {
        TEST_FAIL("should have failed");
        return;
    }
    EXPECT_ERRNO(ENOENT);

    TEST_PASS();
}

/* 21. 目录移动更新 .. */
static void test_dir_move_updates_dotdot(void) {
    TEST_BEGIN("directory move updates ..");

    create_dir(TEST_DIR "/parent1");
    create_dir(TEST_DIR "/parent2");
    create_dir(TEST_DIR "/parent1/child");

    ino_t parent2_inode = get_inode(TEST_DIR "/parent2");

    int ret = rename(TEST_DIR "/parent1/child", TEST_DIR "/parent2/child");
    EXPECT_EQ(0, ret);

    /* 验证 .. 指向新父目录 */
    char dotdot_path[256];
    snprintf(dotdot_path, sizeof(dotdot_path), "%s/parent2/child/..", TEST_DIR);

    ino_t dotdot_inode = get_inode(dotdot_path);
    if (dotdot_inode != parent2_inode) {
        TEST_FAIL(".. should point to new parent (expected %lu, got %lu)",
                  (unsigned long)parent2_inode, (unsigned long)dotdot_inode);
        return;
    }

    TEST_PASS();
}

/* 22. 长文件名 */
static void test_long_filename(void) {
    TEST_BEGIN("long filename rename");

    /* 创建一个接近 NAME_MAX (255) 的文件名 */
    char longname[256];
    memset(longname, 'a', 250);
    longname[250] = '\0';

    char src[512], dst[512];
    snprintf(src, sizeof(src), "%s/%s", TEST_DIR, longname);

    longname[0] = 'b';  /* 修改第一个字符 */
    snprintf(dst, sizeof(dst), "%s/%s", TEST_DIR, longname);

    create_file(src, "data");

    int ret = rename(src, dst);
    EXPECT_EQ(0, ret);

    if (!file_exists(dst)) {
        TEST_FAIL("renamed file should exist");
        return;
    }

    TEST_PASS();
}

/* 23. 特殊字符文件名 */
static void test_special_chars_filename(void) {
    TEST_BEGIN("special characters in filename");

    create_file(TEST_DIR "/file with spaces.txt", "data");

    int ret = rename(TEST_DIR "/file with spaces.txt", TEST_DIR "/new file.txt");
    EXPECT_EQ(0, ret);

    if (!file_exists(TEST_DIR "/new file.txt")) {
        TEST_FAIL("renamed file should exist");
        return;
    }

    TEST_PASS();
}

/* 24. 深层目录移动 */
static void test_deep_dir_move(void) {
    TEST_BEGIN("deep directory tree move");

    create_dir(TEST_DIR "/a");
    create_dir(TEST_DIR "/a/b");
    create_dir(TEST_DIR "/a/b/c");
    create_file(TEST_DIR "/a/b/c/file.txt", "deep");
    create_dir(TEST_DIR "/target");

    int ret = rename(TEST_DIR "/a", TEST_DIR "/target/a");
    EXPECT_EQ(0, ret);

    if (!file_exists(TEST_DIR "/target/a/b/c/file.txt")) {
        TEST_FAIL("deep file should be preserved");
        return;
    }

    TEST_PASS();
}

/* 25. 空文件重命名 */
static void test_empty_file_rename(void) {
    TEST_BEGIN("empty file rename");

    create_file(TEST_DIR "/empty.txt", NULL);

    int ret = rename(TEST_DIR "/empty.txt", TEST_DIR "/empty2.txt");
    EXPECT_EQ(0, ret);

    struct stat st;
    if (stat(TEST_DIR "/empty2.txt", &st) < 0) {
        TEST_FAIL("file should exist");
        return;
    }
    if (st.st_size != 0) {
        TEST_FAIL("file should be empty");
        return;
    }

    TEST_PASS();
}

/* 26. 只读目录中重命名（应该失败） */
static void test_rename_in_readonly_dir(void) {
    TEST_BEGIN("rename in read-only directory");

    if (getuid() == 0) {
        TEST_SKIP("running as root");
        return;
    }

    create_dir(TEST_DIR "/readonly");
    create_file(TEST_DIR "/readonly/file.txt", "data");
    chmod(TEST_DIR "/readonly", 0555);

    int ret = rename(TEST_DIR "/readonly/file.txt", TEST_DIR "/readonly/new.txt");

    chmod(TEST_DIR "/readonly", 0755);  /* 恢复权限以便清理 */

    if (ret == 0) {
        TEST_FAIL("should have failed");
        return;
    }
    EXPECT_ERRNO(EACCES);

    TEST_PASS();
}

/* 27. 跨文件系统重命名（应该失败） */
static void test_cross_filesystem_rename(void) {
    TEST_BEGIN("cross filesystem rename");

    /* /tmp 和 / 通常在不同的文件系统 */
    create_file(TEST_DIR "/file.txt", "data");

    int ret = rename(TEST_DIR "/file.txt", "/cross_fs_test_file.txt");

    if (ret == 0) {
        unlink("/cross_fs_test_file.txt");
        /* 可能在同一文件系统，跳过 */
        TEST_SKIP("same filesystem");
        return;
    }

    if (errno == EXDEV) {
        TEST_PASS();
    } else if (errno == EACCES || errno == EPERM) {
        TEST_SKIP("permission denied");
    } else {
        TEST_FAIL("unexpected error %d: %s", errno, strerror(errno));
    }
}

/* 28. 目录自身重命名 */
static void test_rename_dir_to_itself(void) {
    TEST_BEGIN("rename directory to itself");

    create_dir(TEST_DIR "/mydir");

    int ret = rename(TEST_DIR "/mydir", TEST_DIR "/mydir");
    EXPECT_EQ(0, ret);

    if (!is_dir(TEST_DIR "/mydir")) {
        TEST_FAIL("directory should still exist");
        return;
    }

    TEST_PASS();
}

/* 29. 测试 rename 的原子性（简单检查） */
static void test_rename_atomic_simple(void) {
    TEST_BEGIN("rename atomic (content preserved)");

    /* 写入较大的文件 */
    int fd = open(TEST_DIR "/bigfile.txt", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) {
        TEST_FAIL("cannot create file");
        return;
    }

    char buf[4096];
    memset(buf, 'X', sizeof(buf));
    for (int i = 0; i < 100; i++) {
        ssize_t n = write(fd, buf, sizeof(buf));
        (void)n;
    }
    close(fd);

    struct stat st_before;
    stat(TEST_DIR "/bigfile.txt", &st_before);

    int ret = rename(TEST_DIR "/bigfile.txt", TEST_DIR "/bigfile_renamed.txt");
    EXPECT_EQ(0, ret);

    struct stat st_after;
    if (stat(TEST_DIR "/bigfile_renamed.txt", &st_after) < 0) {
        TEST_FAIL("renamed file missing");
        return;
    }

    if (st_before.st_size != st_after.st_size) {
        TEST_FAIL("size changed during rename");
        return;
    }

    TEST_PASS();
}

/* 30. 用 . 或 .. 作为源或目标 */
static void test_rename_dot_entries(void) {
    TEST_BEGIN("rename . or .. should fail");

    create_dir(TEST_DIR "/testdir");

    /* 尝试重命名 . */
    int ret = rename(TEST_DIR "/testdir/.", TEST_DIR "/newname");
    if (ret == 0) {
        TEST_FAIL("rename . should fail");
        return;
    }
    if (errno != EINVAL && errno != EBUSY) {
        TEST_FAIL("expected EINVAL or EBUSY, got %d", errno);
        return;
    }

    /* 尝试重命名 .. */
    ret = rename(TEST_DIR "/testdir/..", TEST_DIR "/newname");
    if (ret == 0) {
        TEST_FAIL("rename .. should fail");
        return;
    }
    if (errno != EINVAL && errno != EBUSY) {
        TEST_FAIL("expected EINVAL or EBUSY, got %d", errno);
        return;
    }

    TEST_PASS();
}

/* ========== 主函数 ========== */

int main(int argc, char *argv[]) {
    (void)argc;
    (void)argv;

    printf("\n");
    printf("===========================================\n");
    printf("  rename/move System Call Test Suite\n");
    printf("  Test directory: %s\n", TEST_DIR);
    printf("===========================================\n\n");

    /* 基本操作 */
    printf("--- Basic Operations ---\n");
    setup_test_env(); test_basic_file_rename();
    setup_test_env(); test_basic_dir_rename();
    setup_test_env(); test_cross_dir_move_file();
    setup_test_env(); test_rename_same_name();
    setup_test_env(); test_rename_dir_to_itself();

    /* 覆盖操作 */
    printf("\n--- Overwrite Operations ---\n");
    setup_test_env(); test_overwrite_existing_file();
    setup_test_env(); test_overwrite_empty_dir();
    setup_test_env(); test_cannot_overwrite_nonempty_dir();

    /* 错误情况 */
    printf("\n--- Error Cases ---\n");
    setup_test_env(); test_source_not_exist();
    setup_test_env(); test_target_dir_not_exist();
    setup_test_env(); test_file_cannot_overwrite_dir();
    setup_test_env(); test_dir_cannot_overwrite_file();
    setup_test_env(); test_circular_rename();
    setup_test_env(); test_rename_dot_entries();

    /* 硬链接 */
    printf("\n--- Hardlink Cases ---\n");
    setup_test_env(); test_hardlink_same_inode_cross_dir();
    setup_test_env(); test_hardlink_same_inode_same_dir();

    /* 符号链接 */
    printf("\n--- Symlink Cases ---\n");
    setup_test_env(); test_symlink_rename();
    setup_test_env(); test_rename_does_not_follow_symlink();

    /* renameat2 扩展标志 */
    printf("\n--- renameat2 Flags ---\n");
    setup_test_env(); test_rename_noreplace();
    setup_test_env(); test_rename_noreplace_no_target();
    setup_test_env(); test_rename_exchange();
    setup_test_env(); test_rename_exchange_no_target();

    /* 目录特殊情况 */
    printf("\n--- Directory Special Cases ---\n");
    setup_test_env(); test_dir_move_updates_dotdot();
    setup_test_env(); test_deep_dir_move();

    /* 其他 */
    printf("\n--- Misc ---\n");
    setup_test_env(); test_long_filename();
    setup_test_env(); test_special_chars_filename();
    setup_test_env(); test_empty_file_rename();
    setup_test_env(); test_rename_atomic_simple();
    setup_test_env(); test_rename_in_readonly_dir();
    setup_test_env(); test_cross_filesystem_rename();

    /* 清理 */
    remove_recursive(TEST_DIR);

    /* 总结 */
    printf("\n===========================================\n");
    printf("  Results: %d tests, ", tests_run);
    if (tests_failed == 0) {
        printf(COLOR_GREEN "%d passed" COLOR_RESET, tests_passed);
    } else {
        printf(COLOR_GREEN "%d passed" COLOR_RESET ", " COLOR_RED "%d failed" COLOR_RESET,
               tests_passed, tests_failed);
    }
    printf("\n===========================================\n\n");

    return tests_failed > 0 ? 1 : 0;
}
