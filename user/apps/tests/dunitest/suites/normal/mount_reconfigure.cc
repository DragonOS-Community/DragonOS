#include <gtest/gtest.h>

#include <atomic>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <string>
#include <string.h>
#include <sys/mount.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <time.h>
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

struct AtimeReaderContext {
    int fd;
    std::atomic<bool> stop;
};

static void *atime_reader(void *opaque) {
    auto *context = static_cast<AtimeReaderContext *>(opaque);
    char byte;
    while (!context->stop.load(std::memory_order_relaxed)) {
        if (pread(context->fd, &byte, 1, 0) != 1) {
            return reinterpret_cast<void *>(1);
        }
    }
    return nullptr;
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

    errno = 0;
    EXPECT_EQ(-1, access(target, W_OK));
    EXPECT_EQ(EROFS, errno);

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

    int fd = open(target_file, O_RDONLY);
    ASSERT_GE(fd, 0) << strerror(errno);
    errno = 0;
    void *mapping = mmap(NULL, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, fd, 0);
    EXPECT_EQ(MAP_FAILED, mapping);
    EXPECT_EQ(EPERM, errno);

    mapping = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
    ASSERT_NE(MAP_FAILED, mapping) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, mprotect(mapping, 4096, PROT_READ | PROT_EXEC));
    EXPECT_EQ(EACCES, errno);
    EXPECT_EQ(0, munmap(mapping, 4096)) << strerror(errno);
    close(fd);

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

TEST(MountReconfigure, StrictAtimeReadUpdatesAccessTime) {
    const char *target = "/tmp/test_mount_strictatime_read";
    const char *file = "/tmp/test_mount_strictatime_read/file";

    ensure_dir("/tmp");
    ensure_dir(target);
    ASSERT_EQ(0, unshare(CLONE_NEWNS)) << strerror(errno);
    ASSERT_EQ(0, mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", target, "tmpfs", MS_STRICTATIME, NULL)) << strerror(errno);
    ASSERT_EQ(0, write_file(file)) << strerror(errno);

    struct timespec times[2] = {{1, 0}, {2, 0}};
    ASSERT_EQ(0, utimensat(AT_FDCWD, file, times, 0)) << strerror(errno);

    int fd = open(file, O_RDONLY);
    ASSERT_GE(fd, 0) << strerror(errno);
    char byte;
    ASSERT_EQ(1, read(fd, &byte, 1)) << strerror(errno);
    close(fd);

    struct stat st = {};
    ASSERT_EQ(0, stat(file, &st)) << strerror(errno);
    EXPECT_GT(st.st_atim.tv_sec, 1);
    cleanup_mount(target);
}

TEST(MountReconfigure, StrictAtimeEofReadUpdatesAccessTime) {
    const char *target = "/tmp/test_mount_strictatime_eof";
    const char *file = "/tmp/test_mount_strictatime_eof/empty";

    ensure_dir("/tmp");
    ensure_dir(target);
    ASSERT_EQ(0, unshare(CLONE_NEWNS)) << strerror(errno);
    ASSERT_EQ(0, mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", target, "tmpfs", MS_STRICTATIME, NULL)) << strerror(errno);

    int fd = open(file, O_CREAT | O_RDONLY, 0600);
    ASSERT_GE(fd, 0) << strerror(errno);
    struct timespec times[2] = {{1, 0}, {2, 0}};
    ASSERT_EQ(0, utimensat(AT_FDCWD, file, times, 0)) << strerror(errno);

    char byte = 0;
    ASSERT_EQ(0, read(fd, &byte, 1)) << strerror(errno);
    struct stat st = {};
    ASSERT_EQ(0, fstat(fd, &st)) << strerror(errno);
    EXPECT_GT(st.st_atim.tv_sec, 1);

    ASSERT_EQ(0, utimensat(AT_FDCWD, file, times, 0)) << strerror(errno);
    ASSERT_EQ(0, read(fd, &byte, 0)) << strerror(errno);
    ASSERT_EQ(0, fstat(fd, &st)) << strerror(errno);
    EXPECT_EQ(1, st.st_atim.tv_sec);
    close(fd);
    unlink(file);
    cleanup_mount(target);
}

TEST(MountReconfigure, DirectoryReadHonorsAtimeFlags) {
    const char *strict_target = "/tmp/test_mount_dir_strictatime";
    const char *strict_file = "/tmp/test_mount_dir_strictatime/file";
    const char *nodir_target = "/tmp/test_mount_dir_nodiratime";
    const char *nodir_file = "/tmp/test_mount_dir_nodiratime/file";

    ensure_dir("/tmp");
    ensure_dir(strict_target);
    ensure_dir(nodir_target);
    ASSERT_EQ(0, unshare(CLONE_NEWNS)) << strerror(errno);
    ASSERT_EQ(0, mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", strict_target, "tmpfs", MS_STRICTATIME, NULL))
        << strerror(errno);
    ASSERT_EQ(0,
              mount("tmpfs", nodir_target, "tmpfs", MS_STRICTATIME | MS_NODIRATIME, NULL))
        << strerror(errno);
    ASSERT_EQ(0, write_file(strict_file)) << strerror(errno);
    ASSERT_EQ(0, write_file(nodir_file)) << strerror(errno);

    struct timespec times[2] = {{1, 0}, {2, 0}};
    ASSERT_EQ(0, utimensat(AT_FDCWD, strict_target, times, 0)) << strerror(errno);
    ASSERT_EQ(0, utimensat(AT_FDCWD, nodir_target, times, 0)) << strerror(errno);

    auto read_directory = [](const char *path) {
        DIR *dir = opendir(path);
        if (dir == nullptr) {
            return -1;
        }
        while (readdir(dir) != nullptr) {
        }
        return closedir(dir);
    };
    ASSERT_EQ(0, read_directory(strict_target)) << strerror(errno);
    ASSERT_EQ(0, read_directory(nodir_target)) << strerror(errno);

    struct stat strict_st = {};
    struct stat nodir_st = {};
    ASSERT_EQ(0, stat(strict_target, &strict_st)) << strerror(errno);
    ASSERT_EQ(0, stat(nodir_target, &nodir_st)) << strerror(errno);
    EXPECT_GT(strict_st.st_atim.tv_sec, 1);
    EXPECT_EQ(1, nodir_st.st_atim.tv_sec);

    unlink(strict_file);
    unlink(nodir_file);
    cleanup_mount(strict_target);
    cleanup_mount(nodir_target);
}

TEST(MountReconfigure, FollowsTmpfsSymlinkLongerThan256Bytes) {
    const char *target = "/tmp/test_mount_long_symlink";
    const char *file = "/tmp/test_mount_long_symlink/file";
    const char *link = "/tmp/test_mount_long_symlink/link";

    ensure_dir("/tmp");
    ensure_dir(target);
    ASSERT_EQ(0, unshare(CLONE_NEWNS)) << strerror(errno);
    ASSERT_EQ(0, mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", target, "tmpfs", 0, NULL)) << strerror(errno);
    ASSERT_EQ(0, write_file(file)) << strerror(errno);

    std::string link_target;
    for (int i = 0; i < 130; ++i) {
        link_target += "./";
    }
    link_target += "file";
    ASSERT_GT(link_target.size(), 256UL);
    ASSERT_EQ(0, symlink(link_target.c_str(), link)) << strerror(errno);

    int fd = open(link, O_RDONLY);
    ASSERT_GE(fd, 0) << strerror(errno);
    char byte = 0;
    ASSERT_EQ(1, read(fd, &byte, 1)) << strerror(errno);
    EXPECT_EQ('x', byte);
    close(fd);
    unlink(link);
    unlink(file);
    cleanup_mount(target);
}

TEST(MountReconfigure, CreatDirectoryIsAlwaysInvalid) {
    const char *target = "/tmp/test_mount_creat_directory";
    const char *missing = "/tmp/test_mount_creat_directory/missing";

    ensure_dir("/tmp");
    ensure_dir(target);

    errno = 0;
    EXPECT_EQ(-1, open(target, O_RDONLY | O_CREAT | O_DIRECTORY, 0600));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, open(missing, O_RDONLY | O_CREAT | O_DIRECTORY, 0600));
    EXPECT_EQ(EINVAL, errno);
    rmdir(target);
}

TEST(MountReconfigure, NoatimeRequiresOwnerOrCapFowner) {
    const char *target = "/tmp/test_mount_noatime_permission";
    const char *file = "/tmp/test_mount_noatime_permission/file";
    const char *link = "/tmp/test_mount_noatime_permission/link";

    ensure_dir("/tmp");
    ensure_dir(target);
    ASSERT_EQ(0, unshare(CLONE_NEWNS)) << strerror(errno);
    ASSERT_EQ(0, mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", target, "tmpfs", MS_STRICTATIME, NULL)) << strerror(errno);
    ASSERT_EQ(0, write_file(file)) << strerror(errno);
    ASSERT_EQ(0, symlink("file", link)) << strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (setuid(1000) != 0) {
            _exit(10);
        }

        errno = 0;
        int fd = open(file, O_RDONLY | O_NOATIME);
        if (fd >= 0 || errno != EPERM) {
            if (fd >= 0) {
                close(fd);
            }
            _exit(11);
        }

        errno = 0;
        fd = open(link, O_RDONLY | O_NOFOLLOW | O_NOATIME);
        if (fd >= 0 || errno != ELOOP) {
            if (fd >= 0) {
                close(fd);
            }
            _exit(14);
        }

        errno = 0;
        fd = open(target, O_RDONLY | O_DIRECT | O_NOATIME);
        if (fd >= 0 || errno != EPERM) {
            if (fd >= 0) {
                close(fd);
            }
            _exit(15);
        }

        fd = open(file, O_RDONLY);
        if (fd < 0) {
            _exit(12);
        }
        errno = 0;
        if (fcntl(fd, F_SETFL, O_NOATIME) != -1 || errno != EPERM) {
            close(fd);
            _exit(13);
        }
        close(fd);
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    unlink(link);
    cleanup_mount(target);
}

TEST(MountReconfigure, NoatimeCreateUsesCallerOwnership) {
    const char *target = "/tmp/test_mount_noatime_create_owner";
    const char *file = "/tmp/test_mount_noatime_create_owner/file";

    ensure_dir("/tmp");
    ensure_dir(target);
    ASSERT_EQ(0, unshare(CLONE_NEWNS)) << strerror(errno);
    ASSERT_EQ(0, mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", target, "tmpfs", MS_STRICTATIME, "mode=0777"))
        << strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (setgid(1000) != 0 || setuid(1000) != 0) {
            _exit(20);
        }
        int fd = open(file, O_CREAT | O_EXCL | O_RDONLY | O_NOATIME, 0600);
        if (fd < 0) {
            _exit(21);
        }
        struct stat st = {};
        if (fstat(fd, &st) != 0 || st.st_uid != 1000 || st.st_gid != 1000) {
            close(fd);
            _exit(22);
        }
        close(fd);
        fd = open(file, O_RDONLY | O_NOATIME);
        if (fd < 0) {
            _exit(23);
        }
        close(fd);
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    unlink(file);
    cleanup_mount(target);
}

TEST(MountReconfigure, SetgidDirectoryCreationSemantics) {
    const char *target = "/tmp/test_mount_setgid_create";
    const char *parent = "/tmp/test_mount_setgid_create/parent";
    const char *file = "/tmp/test_mount_setgid_create/parent/file";
    const char *dir = "/tmp/test_mount_setgid_create/parent/dir";

    ensure_dir("/tmp");
    ensure_dir(target);
    ASSERT_EQ(0, unshare(CLONE_NEWNS)) << strerror(errno);
    ASSERT_EQ(0, mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", target, "tmpfs", 0, "mode=0777")) << strerror(errno);
    ASSERT_EQ(0, mkdir(parent, 0777)) << strerror(errno);
    ASSERT_EQ(0, chown(parent, 0, 2000)) << strerror(errno);
    ASSERT_EQ(0, chmod(parent, 02777)) << strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (setgid(1000) != 0 || setuid(1000) != 0) {
            _exit(30);
        }
        umask(0);
        int fd = open(file, O_CREAT | O_EXCL | O_WRONLY, 02770);
        if (fd < 0) {
            _exit(31);
        }
        close(fd);
        struct stat st = {};
        if (stat(file, &st) != 0 || st.st_gid != 2000 || (st.st_mode & S_ISGID) != 0) {
            _exit(32);
        }
        if (mkdir(dir, 0770) != 0) {
            _exit(33);
        }
        if (stat(dir, &st) != 0 || st.st_gid != 2000 || (st.st_mode & S_ISGID) == 0) {
            _exit(34);
        }
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    unlink(file);
    rmdir(dir);
    rmdir(parent);
    cleanup_mount(target);
}

TEST(MountReconfigure, AtimeUpdateDoesNotOverwriteConcurrentMetadata) {
    const char *target = "/tmp/test_mount_atime_atomic";
    const char *file = "/tmp/test_mount_atime_atomic/file";

    ensure_dir("/tmp");
    ensure_dir(target);
    ASSERT_EQ(0, unshare(CLONE_NEWNS)) << strerror(errno);
    ASSERT_EQ(0, mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", target, "tmpfs", MS_STRICTATIME, "size=1m"))
        << strerror(errno);
    ASSERT_EQ(0, write_file(file)) << strerror(errno);

    int fd = open(file, O_RDONLY);
    ASSERT_GE(fd, 0) << strerror(errno);
    AtimeReaderContext context = {fd, false};
    pthread_t reader;
    ASSERT_EQ(0, pthread_create(&reader, nullptr, atime_reader, &context));

    for (int i = 0; i < 2000; ++i) {
        ASSERT_EQ(0, chmod(file, (i & 1) ? 0613 : 0642)) << strerror(errno);
        struct timespec times[2] = {{100 + i, 0}, {200 + i, 0}};
        ASSERT_EQ(0, utimensat(AT_FDCWD, file, times, 0)) << strerror(errno);
    }

    const mode_t final_mode = 0613;
    const time_t final_mtime = 123456;
    ASSERT_EQ(0, chmod(file, final_mode)) << strerror(errno);
    struct timespec final_times[2] = {{1, 0}, {final_mtime, 0}};
    ASSERT_EQ(0, utimensat(AT_FDCWD, file, final_times, 0)) << strerror(errno);
    usleep(20000);
    context.stop.store(true, std::memory_order_relaxed);
    void *thread_result = nullptr;
    ASSERT_EQ(0, pthread_join(reader, &thread_result));
    ASSERT_EQ(nullptr, thread_result);

    struct stat st = {};
    ASSERT_EQ(0, stat(file, &st)) << strerror(errno);
    EXPECT_EQ(final_mode, st.st_mode & 0777);
    EXPECT_EQ(final_mtime, st.st_mtim.tv_sec);
    EXPECT_GT(st.st_atim.tv_sec, 1);

    close(fd);
    cleanup_mount(target);
}

TEST(MountReconfigure, TmpfsStatfsTracksPageQuotaWithoutCachedFreeBlocks) {
    const char *target = "/tmp/test_tmpfs_statfs_quota";
    const char *file = "/tmp/test_tmpfs_statfs_quota/file";

    ensure_dir("/tmp");
    ensure_dir(target);
    ASSERT_EQ(0, unshare(CLONE_NEWNS)) << strerror(errno);
    ASSERT_EQ(0, mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", target, "tmpfs", 0, "size=16k")) << strerror(errno);

    struct statfs before = {};
    ASSERT_EQ(0, statfs(target, &before)) << strerror(errno);
    ASSERT_EQ(4UL, before.f_blocks);
    ASSERT_EQ(4UL, before.f_bfree);

    int fd = open(file, O_CREAT | O_RDWR | O_TRUNC, 0600);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(0, posix_fallocate(fd, 0, 8192));

    struct statfs allocated = {};
    ASSERT_EQ(0, statfs(target, &allocated)) << strerror(errno);
    EXPECT_EQ(4UL, allocated.f_blocks);
    EXPECT_EQ(2UL, allocated.f_bfree);

    errno = 0;
    EXPECT_EQ(-1, mount("tmpfs", target, "tmpfs", MS_REMOUNT, "size=4k"));
    EXPECT_EQ(EINVAL, errno);

    struct statfs rejected = {};
    ASSERT_EQ(0, statfs(target, &rejected)) << strerror(errno);
    EXPECT_EQ(4UL, rejected.f_blocks);
    EXPECT_EQ(2UL, rejected.f_bfree);

    ASSERT_EQ(0, ftruncate(fd, 0)) << strerror(errno);
    struct statfs released = {};
    ASSERT_EQ(0, statfs(target, &released)) << strerror(errno);
    EXPECT_EQ(4UL, released.f_bfree);

    close(fd);
    unlink(file);
    cleanup_mount(target);
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

    // Regression: popping the top mount must restore the lower mount's reverse
    // mountpoint inode index. copy_mnt_ns() uses that index when cloning the
    // namespace, so a second unshare should preserve the now-visible lower mount.
    if (unshare(CLONE_NEWNS) != 0) {
        int saved_errno = errno;
        umount(target);
        unlink(sibling);
        rmdir(target);
        rmdir(base);
        FAIL() << strerror(saved_errno);
    }
    EXPECT_TRUE(path_exists(lower_marker));
    EXPECT_FALSE(path_exists(upper_marker));
    EXPECT_TRUE(path_exists(sibling));

    ASSERT_EQ(0, umount(target)) << strerror(errno);
    unlink(sibling);
    rmdir(target);
    rmdir(base);
}

TEST(MountReconfigure, StackedMountRepeatedUnmountKeepsLowerIndex) {
    const char *base = "/tmp/test_stacked_mount_repeated";
    const char *target = "/tmp/test_stacked_mount_repeated/target";
    char lower_marker[256];
    char upper_marker[256];

    ensure_dir("/tmp");
    ensure_dir(base);
    ensure_dir(target);

    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << strerror(errno);
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    for (int i = 0; i < 16; ++i) {
        snprintf(lower_marker, sizeof(lower_marker), "%s/lower_marker_%d", target, i);
        snprintf(upper_marker, sizeof(upper_marker), "%s/upper_marker_%d", target, i);

        ASSERT_EQ(0, mount("", target, "ramfs", 0, NULL)) << strerror(errno);
        ASSERT_EQ(0, write_file(lower_marker)) << strerror(errno);

        ASSERT_EQ(0, mount("", target, "ramfs", 0, NULL)) << strerror(errno);
        ASSERT_EQ(0, write_file(upper_marker)) << strerror(errno);

        ASSERT_EQ(0, umount(target)) << "top umount failed at round " << i << ": "
                                     << strerror(errno);
        EXPECT_TRUE(path_exists(lower_marker)) << "lower mount lost at round " << i;
        EXPECT_FALSE(path_exists(upper_marker)) << "upper mount remained visible at round " << i;

        ASSERT_EQ(0, unshare(CLONE_NEWNS)) << "copy_mnt_ns failed at round " << i << ": "
                                           << strerror(errno);
        EXPECT_TRUE(path_exists(lower_marker)) << "lower mount index lost after unshare at round "
                                               << i;
        EXPECT_FALSE(path_exists(upper_marker)) << "upper mount reappeared after unshare at round "
                                                << i;

        ASSERT_EQ(0, umount(target)) << "lower umount failed at round " << i << ": "
                                     << strerror(errno);
    }

    rmdir(target);
    rmdir(base);
}

TEST(MountReconfigure, BindNamespaceOverOpenFileKeepsOpenFileIoFs) {
    const char *base = "/tmp/test_mount_open_file_io_fs";
    const char *target = "/tmp/test_mount_open_file_io_fs/ipc";
    constexpr size_t kPageSize = 4096;

    ensure_dir("/tmp");
    ensure_dir(base);

    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << strerror(errno);
    }
    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    // tmpfs implements the ordinary file mmap callbacks exercised below;
    // ramfs currently does not and would turn this mount-identity regression
    // into an unrelated default map_pages panic.
    ASSERT_EQ(0, mount("", base, "tmpfs", 0, NULL)) << strerror(errno);
    int fd = open(target, O_CREAT | O_RDWR | O_TRUNC, 0600);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(0, ftruncate(fd, kPageSize)) << strerror(errno);
    ASSERT_EQ(5, pwrite(fd, "lower", 5, 0)) << strerror(errno);
    struct statfs lower_fd_statfs = {};
    ASSERT_EQ(0, fstatfs(fd, &lower_fd_statfs)) << strerror(errno);

    // This is the same bind shape used by Cube agent when it persists IPC
    // namespaces. Path operations must resolve the newly mounted namespace
    // inode, while the already-open fd must retain the ramfs selected at open.
    ASSERT_EQ(0, mount("/proc/self/ns/ipc", target, NULL, MS_BIND, NULL)) << strerror(errno);

    struct statfs overmounted_fd_statfs = {};
    ASSERT_EQ(0, fstatfs(fd, &overmounted_fd_statfs)) << strerror(errno);
    EXPECT_EQ(lower_fd_statfs.f_type, overmounted_fd_statfs.f_type);
    struct statfs overmounted_path_statfs = {};
    ASSERT_EQ(0, statfs(target, &overmounted_path_statfs)) << strerror(errno);
    EXPECT_NE(lower_fd_statfs.f_type, overmounted_path_statfs.f_type);

    void *mapping = mmap(NULL, kPageSize, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    ASSERT_NE(MAP_FAILED, mapping) << strerror(errno);
    EXPECT_EQ(0, memcmp(mapping, "lower", 5));
    static_cast<char *>(mapping)[0] = 'L';
    ASSERT_EQ(0, msync(mapping, kPageSize, MS_SYNC)) << strerror(errno);
    ASSERT_EQ(0, munmap(mapping, kPageSize)) << strerror(errno);

    ASSERT_EQ(0, umount(target)) << strerror(errno);
    char value[5] = {};
    ASSERT_EQ(5, pread(fd, value, sizeof(value), 0)) << strerror(errno);
    EXPECT_EQ(0, memcmp(value, "Lower", 5));
    close(fd);
    unlink(target);
    ASSERT_EQ(0, umount(base)) << strerror(errno);
    rmdir(base);
}

TEST(MountReconfigure, ProcFdMagicLinkKeepsReferencedMountProjection) {
    const char *base = "/tmp/test_mount_proc_fd_projection";
    const char *source = "/tmp/test_mount_proc_fd_projection/source";
    const char *topper = "/tmp/test_mount_proc_fd_projection/topper";
    const char *target = "/tmp/test_mount_proc_fd_projection/target";

    ensure_dir("/tmp");
    ensure_dir(base);
    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << strerror(errno);
    }
    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);
    ASSERT_EQ(0, mount("", base, "tmpfs", 0, NULL)) << strerror(errno);

    int source_fd = open(source, O_CREAT | O_RDWR | O_TRUNC, 0600);
    ASSERT_GE(source_fd, 0) << strerror(errno);
    ASSERT_EQ(6, write(source_fd, "source", 6)) << strerror(errno);
    int topper_fd = open(topper, O_CREAT | O_RDWR | O_TRUNC, 0600);
    ASSERT_GE(topper_fd, 0) << strerror(errno);
    ASSERT_EQ(6, write(topper_fd, "topper", 6)) << strerror(errno);
    close(topper_fd);
    int target_fd = open(target, O_CREAT | O_RDWR | O_TRUNC, 0600);
    ASSERT_GE(target_fd, 0) << strerror(errno);
    close(target_fd);

    char proc_fd[64] = {};
    snprintf(proc_fd, sizeof(proc_fd), "/proc/self/fd/%d", source_fd);
    // The proc-fd magic link retains the lower open file's struct path even
    // after the pathname is covered by a different mount.
    ASSERT_EQ(0, mount(topper, source, NULL, MS_BIND, NULL)) << strerror(errno);
    ASSERT_EQ(0, mount(proc_fd, target, NULL, MS_BIND, NULL)) << strerror(errno);

    struct stat source_stat = {};
    struct stat target_stat = {};
    ASSERT_EQ(0, fstat(source_fd, &source_stat)) << strerror(errno);
    ASSERT_EQ(0, stat(target, &target_stat)) << strerror(errno);
    EXPECT_EQ(source_stat.st_dev, target_stat.st_dev);
    EXPECT_EQ(source_stat.st_ino, target_stat.st_ino);
    char value[6] = {};
    int bound_fd = open(target, O_RDONLY);
    ASSERT_GE(bound_fd, 0) << strerror(errno);
    ASSERT_EQ(6, read(bound_fd, value, sizeof(value))) << strerror(errno);
    EXPECT_EQ(0, memcmp(value, "source", sizeof(value)));
    close(bound_fd);

    ASSERT_EQ(0, umount(target)) << strerror(errno);
    ASSERT_EQ(0, umount(source)) << strerror(errno);
    close(source_fd);
    unlink(source);
    unlink(topper);
    unlink(target);
    ASSERT_EQ(0, umount(base)) << strerror(errno);
    rmdir(base);
}

int main(int argc, char **argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
