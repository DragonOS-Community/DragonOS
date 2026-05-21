#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <sys/xattr.h>
#include <unistd.h>

#ifndef CLONE_NEWNS
#define CLONE_NEWNS 0x00020000
#endif

#ifndef MS_REC
#define MS_REC 16384
#endif

namespace {

static int ensure_dir(const char *path) {
    struct stat st;
    if (stat(path, &st) == 0) {
        return S_ISDIR(st.st_mode) ? 0 : -1;
    }
    return mkdir(path, 0755);
}

static void cleanup_mount(const char *path) {
    umount(path);
    rmdir(path);
}

static int write_file(const char *path) {
    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        return -1;
    }

    int ret = write(fd, "x", 1);
    int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return ret == 1 ? 0 : -1;
}

static bool path_exists(const char *path) {
    struct stat st;
    return stat(path, &st) == 0;
}

static bool mount_has_option(const char *mountpoint, const char *option) {
    FILE *fp;
    char *line = NULL;
    size_t cap = 0;
    bool found = false;

    fp = fopen("/proc/self/mounts", "r");
    if (fp == NULL) {
        return false;
    }

    while (getline(&line, &cap, fp) > 0) {
        char *saveptr = NULL;
        char *field = NULL;
        char *current_mountpoint;
        char *options;
        char *opt_saveptr = NULL;

        field = strtok_r(line, " ", &saveptr);
        if (field == NULL) {
            continue;
        }

        current_mountpoint = strtok_r(NULL, " ", &saveptr);
        if (current_mountpoint == NULL || strcmp(current_mountpoint, mountpoint) != 0) {
            continue;
        }

        strtok_r(NULL, " ", &saveptr);
        options = strtok_r(NULL, " ", &saveptr);
        if (options == NULL) {
            continue;
        }

        for (field = strtok_r(options, ",", &opt_saveptr); field != NULL;
             field = strtok_r(NULL, ",", &opt_saveptr)) {
            if (strcmp(field, option) == 0) {
                found = true;
                break;
            }
        }

        break;
    }

    free(line);
    fclose(fp);
    return found;
}

}  // namespace

TEST(MountReconfigure, BindRemountReadonly) {
    const char *source = "/tmp/test_mount_reconfigure/source";
    const char *target = "/tmp/test_mount_reconfigure/target";
    char src_file[256];
    char dst_file[256];
    int fd;

    ensure_dir("/tmp");
    ensure_dir("/tmp/test_mount_reconfigure");
    ensure_dir(source);
    ensure_dir(target);

    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    if (mount("", source, "ramfs", 0, NULL) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    snprintf(src_file, sizeof(src_file), "%s/source.txt", source);
    fd = open(src_file, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        cleanup_mount(source);
        FAIL() << "create source file failed";
    }
    close(fd);

    if (mount(source, target, NULL, MS_BIND, NULL) != 0) {
        cleanup_mount(source);
        FAIL() << "bind mount failed";
    }

    if (mount(target, target, NULL,
              MS_BIND | MS_REMOUNT | MS_RDONLY | MS_NODEV | MS_NOSUID | MS_NOEXEC,
              NULL) != 0) {
        umount(target);
        cleanup_mount(source);
        FAIL() << strerror(errno);
    }

    snprintf(dst_file, sizeof(dst_file), "%s/readonly.txt", target);
    fd = open(dst_file, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd >= 0) {
        close(fd);
        unlink(dst_file);
        umount(target);
        cleanup_mount(source);
        FAIL() << "readonly bind mount still writable";
    }

    if (errno != EROFS) {
        umount(target);
        cleanup_mount(source);
        FAIL() << "expected EROFS on readonly bind mount";
    }

    umount(target);
    cleanup_mount(source);
}

TEST(MountReconfigure, OrdinaryRemountReadonlyAffectsSharedSuperblock) {
    const char *source = "/tmp/test_mount_remount_superblock/source";
    const char *target = "/tmp/test_mount_remount_superblock/target";
    char path[256];
    struct statfs st;

    ensure_dir("/tmp");
    ensure_dir("/tmp/test_mount_remount_superblock");
    ensure_dir(source);
    ensure_dir(target);

    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    if (mount("", source, "ramfs", 0, NULL) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    snprintf(path, sizeof(path), "%s/file", source);
    ASSERT_EQ(write_file(path), 0) << strerror(errno);

    ASSERT_EQ(mount(source, target, NULL, MS_BIND, NULL), 0) << strerror(errno);

    int held = open(path, O_RDWR);
    ASSERT_GE(held, 0) << strerror(errno);
    errno = 0;
    EXPECT_EQ(mount("", source, "ramfs", MS_REMOUNT | MS_RDONLY, NULL), -1);
    EXPECT_EQ(errno, EBUSY);
    close(held);

    ASSERT_EQ(mount("", source, "ramfs", MS_REMOUNT | MS_RDONLY, NULL), 0) << strerror(errno);

    snprintf(path, sizeof(path), "%s/source_ro", source);
    errno = 0;
    EXPECT_EQ(write_file(path), -1);
    EXPECT_EQ(errno, EROFS);

    snprintf(path, sizeof(path), "%s/bind_ro", target);
    errno = 0;
    EXPECT_EQ(write_file(path), -1);
    EXPECT_EQ(errno, EROFS);

    ASSERT_EQ(statfs(target, &st), 0) << strerror(errno);
    EXPECT_NE(static_cast<unsigned long>(st.f_flags & MS_RDONLY), 0UL);

    ASSERT_EQ(mount("", source, "ramfs", MS_REMOUNT, NULL), 0) << strerror(errno);

    snprintf(path, sizeof(path), "%s/source_rw", source);
    EXPECT_EQ(write_file(path), 0) << strerror(errno);

    umount(target);
    cleanup_mount(source);
}

TEST(MountReconfigure, NodevDeviceOpenAllowsOPath) {
    const char *root = "/tmp/test_mount_nodev_o_path";
    const char *source = "/tmp/test_mount_nodev_o_path/source";
    const char *target = "/tmp/test_mount_nodev_o_path/target";
    char source_dev[256];
    char target_dev[256];
    int fd;

    ensure_dir("/tmp");
    ensure_dir(root);
    ensure_dir(source);
    ensure_dir(target);

    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    snprintf(source_dev, sizeof(source_dev), "%s/null_dev", source);
    snprintf(target_dev, sizeof(target_dev), "%s/null_dev", target);
    if (mknod(source_dev, S_IFCHR | 0600, makedev(1, 3)) != 0) {
        rmdir(target);
        rmdir(source);
        rmdir(root);
        GTEST_SKIP() << strerror(errno);
    }

    if (mount(source, target, NULL, MS_BIND, NULL) != 0) {
        unlink(source_dev);
        rmdir(target);
        rmdir(source);
        rmdir(root);
        FAIL() << strerror(errno);
    }

    if (mount(target, target, NULL, MS_BIND | MS_REMOUNT | MS_NODEV, NULL) != 0) {
        umount(target);
        unlink(source_dev);
        rmdir(target);
        rmdir(source);
        rmdir(root);
        FAIL() << strerror(errno);
    }

    fd = open(target_dev, O_RDONLY);
    if (fd >= 0) {
        close(fd);
        umount(target);
        unlink(source_dev);
        rmdir(target);
        rmdir(source);
        rmdir(root);
        FAIL() << "open without O_PATH unexpectedly succeeded on nodev mount";
    }
    EXPECT_EQ(EACCES, errno);

    fd = open(target_dev, O_PATH);
    if (fd < 0) {
        umount(target);
        unlink(source_dev);
        rmdir(target);
        rmdir(source);
        rmdir(root);
        FAIL() << strerror(errno);
    }

    struct stat st = {};
    ASSERT_EQ(0, fstat(fd, &st)) << strerror(errno);
    EXPECT_TRUE(S_ISCHR(st.st_mode));

    char byte = 0;
    ASSERT_EQ(-1, read(fd, &byte, 1));
    EXPECT_EQ(EBADF, errno);
    close(fd);

    umount(target);
    unlink(source_dev);
    rmdir(target);
    rmdir(source);
    rmdir(root);
}

TEST(MountReconfigure, NoexecRejectsExec) {
    const char *root = "/tmp/test_mount_noexec";
    const char *source = "/tmp/test_mount_noexec/source";
    const char *target = "/tmp/test_mount_noexec/target";
    char source_file[256];
    char target_file[256];

    ensure_dir("/tmp");
    ensure_dir(root);
    ensure_dir(source);
    ensure_dir(target);

    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    snprintf(source_file, sizeof(source_file), "%s/program", source);
    snprintf(target_file, sizeof(target_file), "%s/program", target);
    ASSERT_EQ(0, write_file(source_file)) << strerror(errno);
    ASSERT_EQ(0, chmod(source_file, 0755)) << strerror(errno);

    if (mount(source, target, NULL, MS_BIND, NULL) != 0) {
        unlink(source_file);
        rmdir(target);
        rmdir(source);
        rmdir(root);
        FAIL() << strerror(errno);
    }

    if (mount(target, target, NULL, MS_BIND | MS_REMOUNT | MS_NOEXEC, NULL) != 0) {
        umount(target);
        unlink(source_file);
        rmdir(target);
        rmdir(source);
        rmdir(root);
        FAIL() << strerror(errno);
    }

    char *const argv[] = {target_file, nullptr};
    char *const envp[] = {nullptr};
    errno = 0;
    EXPECT_EQ(-1, execve(target_file, argv, envp));
    EXPECT_EQ(EACCES, errno);

    umount(target);
    unlink(source_file);
    rmdir(target);
    rmdir(source);
    rmdir(root);
}

TEST(MountReconfigure, BindRemountReadonlyDoesNotChangeSourceSuperblock) {
    const char *source = "/tmp/test_bind_remount_scope/source";
    const char *target = "/tmp/test_bind_remount_scope/target";
    char path[256];

    ensure_dir("/tmp");
    ensure_dir("/tmp/test_bind_remount_scope");
    ensure_dir(source);
    ensure_dir(target);

    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    if (mount("", source, "ramfs", 0, NULL) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    ASSERT_EQ(mount(source, target, NULL, MS_BIND, NULL), 0) << strerror(errno);
    ASSERT_EQ(mount(NULL, target, NULL, MS_REMOUNT | MS_BIND | MS_RDONLY, NULL), 0)
        << strerror(errno);

    snprintf(path, sizeof(path), "%s/bind_ro", target);
    errno = 0;
    EXPECT_EQ(write_file(path), -1);
    EXPECT_EQ(errno, EROFS);

    snprintf(path, sizeof(path), "%s/source_rw", source);
    EXPECT_EQ(write_file(path), 0) << strerror(errno);

    umount(target);
    cleanup_mount(source);
}

TEST(MountReconfigure, TmpfsRemountModeCanReturnToDefaultPermissions) {
    const char *target = "/tmp/test_tmpfs_remount_mode";
    struct stat st;

    ensure_dir("/tmp");
    ensure_dir(target);

    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    if (mount("tmpfs", target, "tmpfs", 0, "mode=777,size=8m") != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    ASSERT_EQ(mount("tmpfs", target, "tmpfs", MS_REMOUNT, "mode=755"), 0) << strerror(errno);
    ASSERT_EQ(stat(target, &st), 0) << strerror(errno);
    EXPECT_EQ(st.st_mode & 0777, 0755U);

    ASSERT_EQ(mount("tmpfs", target, "tmpfs", MS_REMOUNT, "mode=777"), 0) << strerror(errno);
    ASSERT_EQ(stat(target, &st), 0) << strerror(errno);
    EXPECT_EQ(st.st_mode & 0777, 0777U);

    cleanup_mount(target);
}

TEST(MountReconfigure, DefaultReconfigureAcceptsOnlyCommonOptions) {
    const char *target = "/tmp/test_default_reconfigure_proc";
    struct statfs st;

    ensure_dir("/tmp");
    ensure_dir(target);

    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    if (mount("proc", target, "proc", 0, NULL) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    ASSERT_EQ(mount("proc", target, "proc", MS_REMOUNT, "ro"), 0) << strerror(errno);
    ASSERT_EQ(statfs(target, &st), 0) << strerror(errno);
    EXPECT_NE(static_cast<unsigned long>(st.f_flags & MS_RDONLY), 0UL);

    ASSERT_EQ(mount("proc", target, "proc", MS_REMOUNT, "rw"), 0) << strerror(errno);
    ASSERT_EQ(statfs(target, &st), 0) << strerror(errno);
    EXPECT_EQ(static_cast<unsigned long>(st.f_flags & MS_RDONLY), 0UL);

    errno = 0;
    EXPECT_EQ(mount("proc", target, "proc", MS_REMOUNT | MS_RDONLY, "unknown_private=1"), -1);
    EXPECT_EQ(errno, EINVAL);

    ASSERT_EQ(statfs(target, &st), 0) << strerror(errno);
    EXPECT_EQ(static_cast<unsigned long>(st.f_flags & MS_RDONLY), 0UL);

    cleanup_mount(target);
}

TEST(MountReconfigure, SelfBindSubdirRemountReadonly) {
    const char *root = "/tmp/test_mount_reconfigure_subdir/root";
    const char *subdir = "/tmp/test_mount_reconfigure_subdir/root/proc_bus";
    char ro_file[256];
    int fd;

    ensure_dir("/tmp");
    ensure_dir("/tmp/test_mount_reconfigure_subdir");
    ensure_dir(root);

    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    if (mount("", root, "ramfs", 0, NULL) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    if (ensure_dir(subdir) != 0) {
        cleanup_mount(root);
        FAIL() << "create subdir failed";
    }

    if (mount(subdir, subdir, NULL, MS_BIND | MS_REC, NULL) != 0) {
        cleanup_mount(root);
        FAIL() << "self bind mount failed";
    }

    if (mount(subdir, subdir, NULL, MS_BIND | MS_REC | MS_REMOUNT | MS_RDONLY, NULL) != 0) {
        umount(subdir);
        cleanup_mount(root);
        FAIL() << strerror(errno);
    }

    snprintf(ro_file, sizeof(ro_file), "%s/ro.txt", subdir);
    fd = open(ro_file, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd >= 0) {
        close(fd);
        unlink(ro_file);
        umount(subdir);
        cleanup_mount(root);
        FAIL() << "self bind remount is still writable";
    }

    if (errno != EROFS) {
        umount(subdir);
        cleanup_mount(root);
        FAIL() << "expected EROFS after self bind remount";
    }

    if (umount(subdir) != 0) {
        cleanup_mount(root);
        FAIL() << "umount(self bind subdir) failed";
    }
    cleanup_mount(root);
}

TEST(MountReconfigure, BindSubdirPreservesSubtreeRoot) {
    const char *root = "/tmp/test_mount_bind_subtree/root";
    const char *subdir = "/tmp/test_mount_bind_subtree/root/subdir";
    const char *target = "/tmp/test_mount_bind_subtree/target";
    char sub_only[256];
    char root_only[256];
    int fd;

    ensure_dir("/tmp");
    ensure_dir("/tmp/test_mount_bind_subtree");
    ensure_dir(root);
    ensure_dir(target);

    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    if (mount("", root, "ramfs", 0, NULL) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    if (ensure_dir(subdir) != 0) {
        cleanup_mount(root);
        FAIL() << "create subdir failed";
    }

    snprintf(sub_only, sizeof(sub_only), "%s/sub_only", subdir);
    fd = open(sub_only, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        cleanup_mount(root);
        FAIL() << "create subdir marker failed";
    }
    close(fd);

    snprintf(root_only, sizeof(root_only), "%s/root_only", root);
    fd = open(root_only, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        cleanup_mount(root);
        FAIL() << "create root marker failed";
    }
    close(fd);

    if (mount(subdir, target, NULL, MS_BIND, NULL) != 0) {
        cleanup_mount(root);
        FAIL() << "bind mount failed";
    }

    snprintf(sub_only, sizeof(sub_only), "%s/sub_only", target);
    snprintf(root_only, sizeof(root_only), "%s/root_only", target);

    if (access(sub_only, F_OK) != 0) {
        umount(target);
        cleanup_mount(root);
        FAIL() << "subdir marker missing from bind target";
    }

    if (access(root_only, F_OK) == 0) {
        umount(target);
        cleanup_mount(root);
        FAIL() << "bind target exposed source root instead of subdir root";
    }

    if (umount(target) != 0) {
        cleanup_mount(root);
        FAIL() << "umount(bind subtree target) failed";
    }
    cleanup_mount(root);
}

TEST(MountReconfigure, BindRemountPreservesNoatime) {
    const char *source = "/tmp/test_mount_reconfigure_atime/source";
    const char *target = "/tmp/test_mount_reconfigure_atime/target";

    ensure_dir("/tmp");
    ensure_dir("/tmp/test_mount_reconfigure_atime");
    ensure_dir(source);
    ensure_dir(target);

    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    if (mount("", source, "ramfs", 0, NULL) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    if (mount(source, target, NULL, MS_BIND, NULL) != 0) {
        cleanup_mount(source);
        FAIL() << "bind mount failed";
    }

    if (mount(target, target, NULL, MS_BIND | MS_REMOUNT | MS_NOATIME, NULL) != 0) {
        umount(target);
        cleanup_mount(source);
        FAIL() << "set noatime on bind mount failed";
    }

    if (!mount_has_option(target, "noatime") || mount_has_option(target, "relatime")) {
        umount(target);
        cleanup_mount(source);
        FAIL() << "bind mount did not enter noatime state";
    }

    if (mount(target, target, NULL, MS_BIND | MS_REMOUNT | MS_RDONLY, NULL) != 0) {
        umount(target);
        cleanup_mount(source);
        FAIL() << "readonly bind remount failed";
    }

    if (!mount_has_option(target, "noatime")) {
        umount(target);
        cleanup_mount(source);
        FAIL() << "readonly bind remount lost noatime";
    }

    if (mount_has_option(target, "relatime")) {
        umount(target);
        cleanup_mount(source);
        FAIL() << "readonly bind remount unexpectedly enabled relatime";
    }

    umount(target);
    cleanup_mount(source);
}

TEST(MountReconfigure, BindRemountRequiresMountRoot) {
    const char *source = "/tmp/test_mount_reconfigure_mount_root/source";
    const char *target = "/tmp/test_mount_reconfigure_mount_root/target";
    const char *source_subdir = "/tmp/test_mount_reconfigure_mount_root/source/subdir";

    ensure_dir("/tmp");
    ensure_dir("/tmp/test_mount_reconfigure_mount_root");
    ensure_dir(source);
    ensure_dir(target);

    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    if (mount("", source, "ramfs", 0, NULL) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    if (ensure_dir(source_subdir) != 0) {
        cleanup_mount(source);
        FAIL() << "create source subdir failed";
    }

    if (mount(source, target, NULL, MS_BIND, NULL) != 0) {
        cleanup_mount(source);
        FAIL() << "bind mount failed";
    }

    if (mount(source_subdir, source_subdir, NULL, MS_BIND | MS_REMOUNT | MS_RDONLY, NULL) == 0) {
        umount(target);
        cleanup_mount(source);
        FAIL() << "bind remount unexpectedly accepted non-root target";
    }

    if (errno != EINVAL) {
        umount(target);
        cleanup_mount(source);
        FAIL() << "expected EINVAL for non-root bind remount target";
    }

    umount(target);
    cleanup_mount(source);
}

TEST(MountReconfigure, BindRemountSetxattrReadonly) {
    const char *source = "/tmp/test_mount_reconfigure_xattr/source";
    const char *target = "/tmp/test_mount_reconfigure_xattr/target";
    char src_file[256];
    char dst_file[256];
    int fd;

    ensure_dir("/tmp");
    ensure_dir("/tmp/test_mount_reconfigure_xattr");
    ensure_dir(source);
    ensure_dir(target);

    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    if (mount("", source, "ramfs", 0, NULL) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    snprintf(src_file, sizeof(src_file), "%s/source.txt", source);
    fd = open(src_file, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        cleanup_mount(source);
        FAIL() << "create source file failed";
    }
    close(fd);

    if (setxattr(src_file, "user.mount_ro", "before", 6, 0) != 0) {
        if (errno == ENOTSUP || errno == ENOSYS) {
            cleanup_mount(source);
            GTEST_SKIP() << "xattr not supported";
        }
        cleanup_mount(source);
        FAIL() << "initial setxattr failed";
    }

    if (mount(source, target, NULL, MS_BIND, NULL) != 0) {
        cleanup_mount(source);
        FAIL() << "bind mount failed";
    }

    if (mount(target, target, NULL, MS_BIND | MS_REMOUNT | MS_RDONLY, NULL) != 0) {
        umount(target);
        cleanup_mount(source);
        FAIL() << "readonly bind remount failed";
    }

    snprintf(dst_file, sizeof(dst_file), "%s/source.txt", target);
    if (setxattr(dst_file, "user.mount_ro", "after", 5, XATTR_REPLACE) == 0) {
        umount(target);
        cleanup_mount(source);
        FAIL() << "readonly bind mount still allowed setxattr";
    }

    if (errno != EROFS) {
        umount(target);
        cleanup_mount(source);
        FAIL() << "expected EROFS for setxattr on readonly bind mount";
    }

    umount(target);
    cleanup_mount(source);
}

TEST(MountReconfigure, BindRemountStrictatimeNotPersisted) {
    const char *source = "/tmp/test_mount_reconfigure_strictatime/source";
    const char *target = "/tmp/test_mount_reconfigure_strictatime/target";

    ensure_dir("/tmp");
    ensure_dir("/tmp/test_mount_reconfigure_strictatime");
    ensure_dir(source);
    ensure_dir(target);

    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    if (mount("", source, "ramfs", 0, NULL) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    if (mount(source, target, NULL, MS_BIND, NULL) != 0) {
        cleanup_mount(source);
        FAIL() << "bind mount failed";
    }

    if (mount(target, target, NULL, MS_BIND | MS_REMOUNT | MS_STRICTATIME, NULL) != 0) {
        umount(target);
        cleanup_mount(source);
        FAIL() << "strictatime bind remount failed";
    }

    if (mount_has_option(target, "strictatime")) {
        umount(target);
        cleanup_mount(source);
        FAIL() << "bind remount unexpectedly persisted strictatime";
    }

    if (mount_has_option(target, "relatime") || mount_has_option(target, "noatime") ||
        mount_has_option(target, "nodiratime")) {
        umount(target);
        cleanup_mount(source);
        FAIL() << "strictatime bind remount left stale atime flags";
    }

    umount(target);
    cleanup_mount(source);
}

TEST(MountReconfigure, StackedMountKeepsOriginalTarget) {
    const char *base = "/tmp/test_stacked_mount_target";
    const char *target = "/tmp/test_stacked_mount_target/target";
    const char *sibling = "/tmp/test_stacked_mount_target/sibling_marker";
    const char *lower_marker = "/tmp/test_stacked_mount_target/target/lower_marker";
    const char *upper_marker = "/tmp/test_stacked_mount_target/target/upper_marker";

    ensure_dir("/tmp");
    ensure_dir(base);
    ensure_dir(target);
    ASSERT_EQ(0, write_file(sibling)) << strerror(errno);

    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    ASSERT_EQ(0, mount("", target, "ramfs", 0, NULL)) << strerror(errno);
    ASSERT_EQ(0, write_file(lower_marker)) << strerror(errno);
    ASSERT_TRUE(path_exists(sibling));

    ASSERT_EQ(0, mount("", target, "ramfs", 0, NULL)) << strerror(errno);
    EXPECT_FALSE(path_exists(lower_marker));
    EXPECT_TRUE(path_exists(sibling));
    ASSERT_EQ(0, write_file(upper_marker)) << strerror(errno);

    ASSERT_EQ(0, umount(target)) << strerror(errno);
    EXPECT_TRUE(path_exists(lower_marker));
    EXPECT_FALSE(path_exists(upper_marker));
    EXPECT_TRUE(path_exists(sibling));

    ASSERT_EQ(0, umount(target)) << strerror(errno);
    unlink(sibling);
    rmdir(target);
    rmdir(base);
}

int main(int argc, char **argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
