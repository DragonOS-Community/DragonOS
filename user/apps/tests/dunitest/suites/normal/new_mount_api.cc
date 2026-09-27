#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

#include <string>

// These x86_64/riscv64/loongarch64 syscall numbers and flags are Linux UAPI.
// Keep the test independent of the libc's optional new-mount-API headers.
#ifndef SYS_open_tree
#define SYS_open_tree 428
#endif
#ifndef SYS_move_mount
#define SYS_move_mount 429
#endif
#ifndef SYS_fsopen
#define SYS_fsopen 430
#endif
#ifndef SYS_fsconfig
#define SYS_fsconfig 431
#endif
#ifndef SYS_fsmount
#define SYS_fsmount 432
#endif
#ifndef CLONE_NEWNS
#define CLONE_NEWNS 0x00020000
#endif
#ifndef MS_REC
#define MS_REC 16384
#endif
#ifndef MS_PRIVATE
#define MS_PRIVATE (1 << 18)
#endif
#ifndef MS_SHARED
#define MS_SHARED (1 << 20)
#endif
#ifndef AT_RECURSIVE
#define AT_RECURSIVE 0x8000
#endif

namespace {

constexpr unsigned int kOpenTreeClone = 1;
constexpr unsigned int kOpenTreeCloexec = O_CLOEXEC;
constexpr unsigned int kMoveMountFromEmptyPath = 0x4;
constexpr unsigned int kFsopenCloexec = 1;
constexpr unsigned int kFsmountCloexec = 1;
constexpr unsigned int kFsconfigSetString = 1;
constexpr unsigned int kFsconfigCreate = 6;

int open_tree_at(int dirfd, const char* path, unsigned int flags) {
    return static_cast<int>(syscall(SYS_open_tree, dirfd, path, flags));
}

int fsopen_type(const char* type, unsigned int flags) {
    return static_cast<int>(syscall(SYS_fsopen, type, flags));
}

int fsconfig_call(int fd, unsigned int command, const char* key, const void* value, int aux) {
    return static_cast<int>(syscall(SYS_fsconfig, fd, command, key, value, aux));
}

int fsmount_fd(int fsfd, unsigned int flags, unsigned int attr_flags) {
    return static_cast<int>(syscall(SYS_fsmount, fsfd, flags, attr_flags));
}

int move_mount_at(int from_fd, const char* from_path, int to_fd, const char* to_path,
                  unsigned int flags) {
    return static_cast<int>(syscall(SYS_move_mount, from_fd, from_path, to_fd, to_path, flags));
}

bool exists(const std::string& path) {
    struct stat st = {};
    return stat(path.c_str(), &st) == 0;
}

bool is_mountpoint(const std::string& path) {
    FILE* fp = fopen("/proc/self/mountinfo", "r");
    if (fp == nullptr) {
        return false;
    }
    char line[2048] = {};
    char mountpoint[256] = {};
    bool found = false;
    while (fgets(line, sizeof(line), fp) != nullptr) {
        if (sscanf(line, "%*s %*s %*s %*s %255s", mountpoint) == 1 &&
            path == mountpoint) {
            found = true;
            break;
        }
    }
    fclose(fp);
    return found;
}

int make_dir(const std::string& path) {
    return mkdir(path.c_str(), 0755);
}

int write_marker(const std::string& path) {
    int fd = open(path.c_str(), O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        return -1;
    }
    const int ret = write(fd, "x", 1);
    const int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return ret == 1 ? 0 : -1;
}

class NewMountApiTest : public ::testing::Test {
protected:
    std::string root_;

    void SetUp() override {
        struct stat st = {};
        if (stat("/tmp", &st) != 0) {
            ASSERT_EQ(0, mkdir("/tmp", 0777)) << strerror(errno);
        }
        if (unshare(CLONE_NEWNS) != 0) {
            GTEST_SKIP() << "mount namespace unavailable: " << strerror(errno);
        }
        ASSERT_EQ(0, mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr))
            << strerror(errno);
        root_ = "/tmp/new_mount_api_" + std::to_string(getpid());
        ASSERT_EQ(0, make_dir(root_)) << strerror(errno);
    }

    void TearDown() override {
        if (root_.empty()) {
            return;
        }
        for (const char* suffix : {"/dst/later", "/src/later", "/dst/child",
                                   "/src/child", "/dst", "/src"}) {
            const std::string path = root_ + suffix;
            if (umount(path.c_str()) != 0 && errno != EINVAL && errno != ENOENT) {
                ADD_FAILURE() << "umount " << path << ": " << strerror(errno);
            }
            const std::string marker = path + "/marker";
            unlink(marker.c_str());
            rmdir(path.c_str());
        }
        rmdir(root_.c_str());
    }

    std::string src() const { return root_ + "/src"; }
    std::string dst() const { return root_ + "/dst"; }

    int create_tmpfs_mount_fd() {
        const int fsfd = fsopen_type("tmpfs", kFsopenCloexec);
        if (fsfd < 0) {
            return -1;
        }
        if (fsconfig_call(fsfd, kFsconfigSetString, "source", "new_mount_api", 0) != 0) {
            const int saved_errno = errno;
            close(fsfd);
            errno = saved_errno;
            return -1;
        }
        if (fsconfig_call(fsfd, kFsconfigCreate, nullptr, nullptr, 0) != 0) {
            const int saved_errno = errno;
            close(fsfd);
            errno = saved_errno;
            return -1;
        }
        const int treefd = fsmount_fd(fsfd, kFsmountCloexec, 0);
        const int saved_errno = errno;
        close(fsfd);
        errno = saved_errno;
        return treefd;
    }
};

TEST_F(NewMountApiTest, OpenTreeOrdinaryFdAndFlagValidation) {
    ASSERT_EQ(0, make_dir(src())) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", src().c_str(), "tmpfs", 0, nullptr)) << strerror(errno);

    const int pathfd = open_tree_at(AT_FDCWD, src().c_str(), kOpenTreeCloexec);
    ASSERT_GE(pathfd, 0) << strerror(errno);
    struct stat st = {};
    EXPECT_EQ(0, fstat(pathfd, &st));
    EXPECT_TRUE(S_ISDIR(st.st_mode));
    EXPECT_NE(0, fcntl(pathfd, F_GETFD) & FD_CLOEXEC);
    close(pathfd);

    const int dirfd = open(src().c_str(), O_PATH | O_CLOEXEC);
    ASSERT_GE(dirfd, 0) << strerror(errno);
    const int empty_path_fd = open_tree_at(dirfd, "", AT_EMPTY_PATH);
    ASSERT_GE(empty_path_fd, 0) << strerror(errno);
    EXPECT_EQ(0, fstat(empty_path_fd, &st));
    close(empty_path_fd);
    close(dirfd);

    errno = 0;
    EXPECT_EQ(-1, open_tree_at(AT_FDCWD, src().c_str(), 0x40000000U));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, open_tree_at(AT_FDCWD, src().c_str(), AT_RECURSIVE));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, open_tree_at(-1, "", AT_EMPTY_PATH));
    EXPECT_EQ(EBADF, errno);
}

TEST_F(NewMountApiTest, FscontextValidationAndPhase) {
    errno = 0;
    EXPECT_EQ(-1, fsopen_type("tmpfs", 0x2));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, fsopen_type("not_a_filesystem_type", 0));
    EXPECT_EQ(ENODEV, errno);

    const int fsfd = fsopen_type("tmpfs", kFsopenCloexec);
    ASSERT_GE(fsfd, 0) << strerror(errno);
    EXPECT_NE(0, fcntl(fsfd, F_GETFD) & FD_CLOEXEC);

    errno = 0;
    EXPECT_EQ(-1, fsmount_fd(fsfd, 0, 0));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, fsconfig_call(fsfd, kFsconfigSetString, "size", nullptr, 0));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, fsconfig_call(-1, kFsconfigCreate, nullptr, nullptr, 0));
    EXPECT_EQ(EINVAL, errno);
    const int ordinaryfd = open("/dev/null", O_RDONLY);
    ASSERT_GE(ordinaryfd, 0) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, fsconfig_call(ordinaryfd, kFsconfigCreate, nullptr, nullptr, 0));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, fsmount_fd(ordinaryfd, 0, 0));
    EXPECT_EQ(EINVAL, errno);
    close(ordinaryfd);
    errno = 0;
    EXPECT_EQ(-1, fsmount_fd(-1, 0, 0));
    EXPECT_EQ(EBADF, errno);

    ASSERT_EQ(0, fsconfig_call(fsfd, kFsconfigCreate, nullptr, nullptr, 0)) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, fsconfig_call(fsfd, kFsconfigCreate, nullptr, nullptr, 0));
    EXPECT_EQ(EBUSY, errno);
    errno = 0;
    EXPECT_EQ(-1, fsconfig_call(fsfd, kFsconfigSetString, "size", "65536", 0));
    EXPECT_EQ(EBUSY, errno);
    errno = 0;
    EXPECT_EQ(-1, fsmount_fd(fsfd, 0x2, 0));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, fsmount_fd(fsfd, 0, 0x80000000U));
    EXPECT_EQ(EINVAL, errno);

    const int treefd = fsmount_fd(fsfd, 0, 0);
    ASSERT_GE(treefd, 0) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, fsmount_fd(fsfd, 0, 0));
    EXPECT_EQ(EBUSY, errno);
    close(treefd);
    close(fsfd);
}

TEST_F(NewMountApiTest, FsopenFsmountMoveMountPublishesTree) {
    ASSERT_EQ(0, make_dir(dst())) << strerror(errno);
    const int treefd = create_tmpfs_mount_fd();
    ASSERT_GE(treefd, 0) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, move_mount_at(treefd, "", AT_FDCWD, dst().c_str(), 0));
    EXPECT_EQ(ENOENT, errno);
    ASSERT_EQ(0, move_mount_at(treefd, "", AT_FDCWD, dst().c_str(),
                               kMoveMountFromEmptyPath))
        << strerror(errno);
    ASSERT_EQ(0, write_marker(dst() + "/marker")) << strerror(errno);
    close(treefd);  // Closing a published tree fd must not unmount it.
    EXPECT_TRUE(exists(dst() + "/marker"));
    EXPECT_TRUE(is_mountpoint(dst()));
}

TEST_F(NewMountApiTest, FailedMoveLeavesDetachedFdRetryable) {
    ASSERT_EQ(0, make_dir(dst())) << strerror(errno);
    const int treefd = create_tmpfs_mount_fd();
    ASSERT_GE(treefd, 0) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, move_mount_at(treefd, "", AT_FDCWD, (root_ + "/missing").c_str(),
                                kMoveMountFromEmptyPath));
    EXPECT_EQ(ENOENT, errno);
    EXPECT_FALSE(is_mountpoint(dst()));

    ASSERT_EQ(0, move_mount_at(treefd, "", AT_FDCWD, dst().c_str(),
                               kMoveMountFromEmptyPath))
        << strerror(errno);
    close(treefd);
    EXPECT_TRUE(is_mountpoint(dst()));
}

TEST_F(NewMountApiTest, MoveMountDirectoryTypeMismatchReturnsEinval) {
    const std::string target = root_ + "/file";
    ASSERT_EQ(0, write_marker(target)) << strerror(errno);
    const int treefd = create_tmpfs_mount_fd();
    ASSERT_GE(treefd, 0) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, move_mount_at(treefd, "", AT_FDCWD, target.c_str(),
                                kMoveMountFromEmptyPath));
    EXPECT_EQ(EINVAL, errno);
    close(treefd);
    unlink(target.c_str());
}

TEST_F(NewMountApiTest, AttachedTreeFdCanMoveAgain) {
    ASSERT_EQ(0, make_dir(src())) << strerror(errno);
    ASSERT_EQ(0, make_dir(dst())) << strerror(errno);
    const int treefd = create_tmpfs_mount_fd();
    ASSERT_GE(treefd, 0) << strerror(errno);
    ASSERT_EQ(0, move_mount_at(treefd, "", AT_FDCWD, dst().c_str(),
                               kMoveMountFromEmptyPath))
        << strerror(errno);
    ASSERT_EQ(0, write_marker(dst() + "/marker")) << strerror(errno);

    ASSERT_EQ(0, move_mount_at(treefd, "", AT_FDCWD, src().c_str(),
                               kMoveMountFromEmptyPath))
        << strerror(errno);
    close(treefd);
    EXPECT_FALSE(is_mountpoint(dst()));
    EXPECT_TRUE(is_mountpoint(src()));
    EXPECT_FALSE(exists(dst() + "/marker"));
    EXPECT_TRUE(exists(src() + "/marker"));
}

TEST_F(NewMountApiTest, DupAndForkShareFscontextState) {
    ASSERT_EQ(0, make_dir(dst())) << strerror(errno);
    const int fsfd = fsopen_type("tmpfs", 0);
    ASSERT_GE(fsfd, 0) << strerror(errno);
    const int duplicate = dup(fsfd);
    ASSERT_GE(duplicate, 0) << strerror(errno);
    close(fsfd);

    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        _exit(fsconfig_call(duplicate, kFsconfigCreate, nullptr, nullptr, 0) == 0 ? 0 : 1);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));

    const int treefd = fsmount_fd(duplicate, 0, 0);
    ASSERT_GE(treefd, 0) << strerror(errno);
    close(duplicate);
    ASSERT_EQ(0, move_mount_at(treefd, "", AT_FDCWD, dst().c_str(),
                               kMoveMountFromEmptyPath))
        << strerror(errno);
    close(treefd);
    EXPECT_EQ(0, write_marker(dst() + "/marker")) << strerror(errno);
}

TEST_F(NewMountApiTest, DupKeepsDetachedTreeAlive) {
    ASSERT_EQ(0, make_dir(dst())) << strerror(errno);
    const int treefd = create_tmpfs_mount_fd();
    ASSERT_GE(treefd, 0) << strerror(errno);
    const int duplicate = dup(treefd);
    ASSERT_GE(duplicate, 0) << strerror(errno);
    close(treefd);

    ASSERT_EQ(0, move_mount_at(duplicate, "", AT_FDCWD, dst().c_str(),
                               kMoveMountFromEmptyPath))
        << strerror(errno);
    close(duplicate);
    EXPECT_EQ(0, write_marker(dst() + "/marker")) << strerror(errno);
}

TEST_F(NewMountApiTest, ClosingUnpublishedTreeDoesNotChangeNamespace) {
    ASSERT_EQ(0, make_dir(dst())) << strerror(errno);
    const int treefd = create_tmpfs_mount_fd();
    ASSERT_GE(treefd, 0) << strerror(errno);
    close(treefd);

    EXPECT_EQ(0, write_marker(dst() + "/marker")) << strerror(errno);
    EXPECT_TRUE(exists(dst() + "/marker"));
}

TEST_F(NewMountApiTest, InteriorFileFdSurvivesDetachedRootClose) {
    const int treefd = create_tmpfs_mount_fd();
    ASSERT_GE(treefd, 0) << strerror(errno);
    const int dirfd = openat(treefd, ".", O_RDONLY | O_DIRECTORY);
    ASSERT_GE(dirfd, 0) << strerror(errno);
    const int filefd = openat(dirfd, "marker", O_CREAT | O_RDWR, 0644);
    ASSERT_GE(filefd, 0) << strerror(errno);
    ASSERT_EQ(1, write(filefd, "x", 1)) << strerror(errno);

    close(treefd);
    ASSERT_EQ(0, lseek(filefd, 0, SEEK_SET)) << strerror(errno);
    char data = 0;
    ASSERT_EQ(1, read(filefd, &data, 1)) << strerror(errno);
    EXPECT_EQ('x', data);

    const int reopened = openat(dirfd, "marker", O_RDONLY);
    ASSERT_GE(reopened, 0) << strerror(errno);
    data = 0;
    EXPECT_EQ(1, read(reopened, &data, 1)) << strerror(errno);
    EXPECT_EQ('x', data);
    close(reopened);
    close(filefd);
    close(dirfd);
}

TEST_F(NewMountApiTest, ForkInheritsDetachedTreeFd) {
    ASSERT_EQ(0, make_dir(dst())) << strerror(errno);
    const int treefd = create_tmpfs_mount_fd();
    ASSERT_GE(treefd, 0) << strerror(errno);
    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        _exit(move_mount_at(treefd, "", AT_FDCWD, dst().c_str(),
                            kMoveMountFromEmptyPath) == 0
                  ? 0
                  : 1);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    close(treefd);
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));
    EXPECT_EQ(0, write_marker(dst() + "/marker")) << strerror(errno);
}

TEST_F(NewMountApiTest, OpenTreeCloneKeepsOriginalMount) {
    ASSERT_EQ(0, make_dir(src())) << strerror(errno);
    ASSERT_EQ(0, make_dir(dst())) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", src().c_str(), "tmpfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, write_marker(src() + "/marker")) << strerror(errno);

    const int treefd = open_tree_at(AT_FDCWD, src().c_str(), kOpenTreeClone);
    ASSERT_GE(treefd, 0) << strerror(errno);
    ASSERT_EQ(0, move_mount_at(treefd, "", AT_FDCWD, dst().c_str(),
                               kMoveMountFromEmptyPath))
        << strerror(errno);
    close(treefd);
    EXPECT_TRUE(exists(src() + "/marker"));
    EXPECT_TRUE(exists(dst() + "/marker"));
}

TEST_F(NewMountApiTest, CloneFromSubdirectoryUsesSelectedRoot) {
    ASSERT_EQ(0, make_dir(src())) << strerror(errno);
    ASSERT_EQ(0, make_dir(dst())) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", src().c_str(), "tmpfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, make_dir(src() + "/selected")) << strerror(errno);
    ASSERT_EQ(0, write_marker(src() + "/selected/marker")) << strerror(errno);
    ASSERT_EQ(0, write_marker(src() + "/outside")) << strerror(errno);

    const int treefd = open_tree_at(AT_FDCWD, (src() + "/selected").c_str(), kOpenTreeClone);
    ASSERT_GE(treefd, 0) << strerror(errno);
    ASSERT_EQ(0, move_mount_at(treefd, "", AT_FDCWD, dst().c_str(),
                               kMoveMountFromEmptyPath))
        << strerror(errno);
    close(treefd);
    EXPECT_TRUE(exists(dst() + "/marker"));
    EXPECT_FALSE(exists(dst() + "/outside"));
    EXPECT_TRUE(exists(src() + "/outside"));
}

TEST_F(NewMountApiTest, RecursiveCloneIncludesNestedMount) {
    ASSERT_EQ(0, make_dir(src())) << strerror(errno);
    ASSERT_EQ(0, make_dir(dst())) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", src().c_str(), "tmpfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, make_dir(src() + "/child")) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", (src() + "/child").c_str(), "tmpfs", 0, nullptr))
        << strerror(errno);
    ASSERT_EQ(0, write_marker(src() + "/child/marker")) << strerror(errno);

    const int treefd = open_tree_at(AT_FDCWD, src().c_str(), kOpenTreeClone | AT_RECURSIVE);
    ASSERT_GE(treefd, 0) << strerror(errno);
    ASSERT_EQ(0, move_mount_at(treefd, "", AT_FDCWD, dst().c_str(),
                               kMoveMountFromEmptyPath))
        << strerror(errno);
    close(treefd);
    EXPECT_TRUE(exists(src() + "/child/marker"));
    EXPECT_TRUE(exists(dst() + "/child/marker"));
}

TEST_F(NewMountApiTest, DetachedSharedCloneIgnoresPropagationUntilAttached) {
    ASSERT_EQ(0, make_dir(src())) << strerror(errno);
    ASSERT_EQ(0, make_dir(dst())) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", src().c_str(), "tmpfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, make_dir(src() + "/child")) << strerror(errno);
    ASSERT_EQ(0, make_dir(src() + "/later")) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, src().c_str(), nullptr, MS_SHARED, nullptr)) << strerror(errno);

    const int treefd = open_tree_at(AT_FDCWD, src().c_str(), kOpenTreeClone | AT_RECURSIVE);
    ASSERT_GE(treefd, 0) << strerror(errno);

    // The clone shares a peer group with src, but its anonymous namespace must
    // not receive new child mounts while it is detached (Linux IS_MNT_NEW).
    ASSERT_EQ(0, mount("tmpfs", (src() + "/child").c_str(), "tmpfs", 0, nullptr))
        << strerror(errno);
    ASSERT_EQ(0, write_marker(src() + "/child/marker")) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, openat(treefd, "child/marker", O_RDONLY));
    EXPECT_EQ(ENOENT, errno);

    ASSERT_EQ(0, move_mount_at(treefd, "", AT_FDCWD, dst().c_str(),
                               kMoveMountFromEmptyPath))
        << strerror(errno);
    close(treefd);
    EXPECT_FALSE(exists(dst() + "/child/marker"));

    // Once attached, the same clone is a live shared peer and receives later
    // mount events from src according to ordinary propagation rules.
    ASSERT_EQ(0, mount("tmpfs", (src() + "/later").c_str(), "tmpfs", 0, nullptr))
        << strerror(errno);
    ASSERT_EQ(0, write_marker(src() + "/later/marker")) << strerror(errno);
    EXPECT_TRUE(exists(dst() + "/later/marker"));
}

TEST_F(NewMountApiTest, OverlayFsconfigPinsLayerAtParameterTime) {
    ASSERT_EQ(0, make_dir(src())) << strerror(errno);
    ASSERT_EQ(0, make_dir(dst())) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", src().c_str(), "tmpfs", 0, nullptr)) << strerror(errno);
    const std::string lower = src() + "/lower";
    const std::string moved_lower = src() + "/moved_lower";
    const std::string upper = src() + "/upper";
    const std::string work = src() + "/work";
    ASSERT_EQ(0, make_dir(lower)) << strerror(errno);
    ASSERT_EQ(0, make_dir(upper)) << strerror(errno);
    ASSERT_EQ(0, make_dir(work)) << strerror(errno);
    ASSERT_EQ(0, write_marker(lower + "/marker")) << strerror(errno);

    const int fsfd = fsopen_type("overlay", kFsopenCloexec);
    ASSERT_GE(fsfd, 0) << strerror(errno);
    ASSERT_EQ(0, fsconfig_call(fsfd, kFsconfigSetString, "upperdir", upper.c_str(), 0))
        << strerror(errno);
    ASSERT_EQ(0, fsconfig_call(fsfd, kFsconfigSetString, "workdir", work.c_str(), 0))
        << strerror(errno);
    ASSERT_EQ(0, fsconfig_call(fsfd, kFsconfigSetString, "lowerdir", lower.c_str(), 0))
        << strerror(errno);

    // Linux pins each layer when fsconfig receives it. Replacing the path
    // before CREATE must not silently select the new empty directory.
    ASSERT_EQ(0, rename(lower.c_str(), moved_lower.c_str())) << strerror(errno);
    ASSERT_EQ(0, make_dir(lower)) << strerror(errno);
    ASSERT_EQ(0, fsconfig_call(fsfd, kFsconfigCreate, nullptr, nullptr, 0))
        << strerror(errno);
    const int treefd = fsmount_fd(fsfd, kFsmountCloexec, 0);
    close(fsfd);
    ASSERT_GE(treefd, 0) << strerror(errno);
    ASSERT_EQ(0, move_mount_at(treefd, "", AT_FDCWD, dst().c_str(),
                               kMoveMountFromEmptyPath))
        << strerror(errno);
    close(treefd);
    EXPECT_TRUE(exists(dst() + "/marker"));
}

TEST_F(NewMountApiTest, OverlayLowerOnlyMountIsReadOnly) {
    ASSERT_EQ(0, make_dir(src())) << strerror(errno);
    ASSERT_EQ(0, make_dir(dst())) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", src().c_str(), "tmpfs", 0, nullptr)) << strerror(errno);
    const std::string lower1 = src() + "/lower1";
    const std::string lower2 = src() + "/lower2";
    ASSERT_EQ(0, make_dir(lower1)) << strerror(errno);
    ASSERT_EQ(0, make_dir(lower2)) << strerror(errno);
    ASSERT_EQ(0, write_marker(lower1 + "/marker")) << strerror(errno);

    const int fsfd = fsopen_type("overlay", kFsopenCloexec);
    ASSERT_GE(fsfd, 0) << strerror(errno);
    const std::string lowers = lower1 + ":" + lower2;
    ASSERT_EQ(0, fsconfig_call(fsfd, kFsconfigSetString, "lowerdir", lowers.c_str(), 0))
        << strerror(errno);
    ASSERT_EQ(0, fsconfig_call(fsfd, kFsconfigCreate, nullptr, nullptr, 0))
        << strerror(errno);
    const int treefd = fsmount_fd(fsfd, kFsmountCloexec, 0);
    close(fsfd);
    ASSERT_GE(treefd, 0) << strerror(errno);
    ASSERT_EQ(0, move_mount_at(treefd, "", AT_FDCWD, dst().c_str(),
                               kMoveMountFromEmptyPath)) << strerror(errno);
    close(treefd);
    EXPECT_TRUE(exists(dst() + "/marker"));

    struct statvfs fsstat = {};
    ASSERT_EQ(0, statvfs(dst().c_str(), &fsstat)) << strerror(errno);
    EXPECT_NE(0UL, fsstat.f_flag & ST_RDONLY);

    errno = 0;
    EXPECT_EQ(-1, open((dst() + "/marker").c_str(), O_WRONLY));
    EXPECT_EQ(EROFS, errno);
    const int readonly_fd = open((dst() + "/marker").c_str(), O_RDONLY);
    ASSERT_GE(readonly_fd, 0) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, fchmod(readonly_fd, 0600));
    EXPECT_EQ(EROFS, errno);
    errno = 0;
    EXPECT_EQ(-1, fsetxattr(readonly_fd, "user.lower_only", "x", 1, 0));
    EXPECT_EQ(EROFS, errno);
    errno = 0;
    EXPECT_EQ(MAP_FAILED, mmap(nullptr, 4096, PROT_READ | PROT_WRITE, MAP_SHARED,
                               readonly_fd, 0));
    EXPECT_EQ(EACCES, errno);
    void* readable_map = mmap(nullptr, 4096, PROT_READ, MAP_SHARED, readonly_fd, 0);
    ASSERT_NE(MAP_FAILED, readable_map) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, mprotect(readable_map, 4096, PROT_READ | PROT_WRITE));
    EXPECT_EQ(EACCES, errno);
    EXPECT_EQ(0, munmap(readable_map, 4096));
    close(readonly_fd);
    errno = 0;
    EXPECT_EQ(-1, mkdir((dst() + "/new").c_str(), 0700));
    EXPECT_EQ(EROFS, errno);
    errno = 0;
    EXPECT_EQ(-1, truncate((dst() + "/marker").c_str(), 0));
    EXPECT_EQ(EROFS, errno);
    errno = 0;
    EXPECT_EQ(-1, link((dst() + "/marker").c_str(), (dst() + "/link").c_str()));
    EXPECT_EQ(EROFS, errno);
    errno = 0;
    EXPECT_EQ(-1, rename((dst() + "/marker").c_str(), (dst() + "/renamed").c_str()));
    EXPECT_EQ(EROFS, errno);
    errno = 0;
    EXPECT_EQ(-1, mount(nullptr, dst().c_str(), nullptr, MS_REMOUNT, nullptr));
    EXPECT_EQ(EROFS, errno);
    // Linux ignores legacy overlay-private options on a read-only remount.
    EXPECT_EQ(0, mount(nullptr, dst().c_str(), nullptr, MS_REMOUNT | MS_RDONLY,
                       "lowerdir=/does-not-exist")) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, mount(nullptr, dst().c_str(), nullptr, MS_REMOUNT,
                        "lowerdir=/does-not-exist"));
    EXPECT_EQ(EROFS, errno);
    EXPECT_TRUE(exists(lower1 + "/marker"));
    const std::string bound = src() + "/bound";
    ASSERT_EQ(0, make_dir(bound)) << strerror(errno);
    ASSERT_EQ(0, mount(dst().c_str(), bound.c_str(), nullptr, MS_BIND, nullptr))
        << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, open((bound + "/marker").c_str(), O_WRONLY));
    EXPECT_EQ(EROFS, errno);
    errno = 0;
    EXPECT_EQ(-1, mount(nullptr, bound.c_str(), nullptr, MS_REMOUNT, nullptr));
    EXPECT_EQ(EROFS, errno);
    EXPECT_EQ(0, umount(bound.c_str())) << strerror(errno);
    EXPECT_EQ(0, rmdir(bound.c_str())) << strerror(errno);
    struct stat lower_stat = {};
    ASSERT_EQ(0, stat((lower1 + "/marker").c_str(), &lower_stat));
    EXPECT_EQ(0644U, lower_stat.st_mode & 0777);
    EXPECT_EQ(1, lower_stat.st_size);
}

TEST_F(NewMountApiTest, OverlayLowerOnlyLegacyMount) {
    ASSERT_EQ(0, make_dir(src())) << strerror(errno);
    ASSERT_EQ(0, make_dir(dst())) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", src().c_str(), "tmpfs", 0, nullptr)) << strerror(errno);
    const std::string lower1 = src() + "/lower1";
    const std::string lower2 = src() + "/lower2";
    ASSERT_EQ(0, make_dir(lower1)) << strerror(errno);
    ASSERT_EQ(0, make_dir(lower2)) << strerror(errno);
    ASSERT_EQ(0, write_marker(lower1 + "/marker")) << strerror(errno);
    const std::string options = "lowerdir=" + lower1 + ":" + lower2;
    ASSERT_EQ(0, mount("overlay", dst().c_str(), "overlay", 0, options.c_str()))
        << strerror(errno);
    EXPECT_TRUE(exists(dst() + "/marker"));
    errno = 0;
    EXPECT_EQ(-1, mkdir((dst() + "/new").c_str(), 0700));
    EXPECT_EQ(EROFS, errno);
}

TEST_F(NewMountApiTest, OverlayLowerOnlyRequiresTwoLayers) {
    ASSERT_EQ(0, make_dir(src())) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", src().c_str(), "tmpfs", 0, nullptr)) << strerror(errno);
    const std::string lower = src() + "/lower";
    ASSERT_EQ(0, make_dir(lower)) << strerror(errno);
    const int fsfd = fsopen_type("overlay", kFsopenCloexec);
    ASSERT_GE(fsfd, 0) << strerror(errno);
    ASSERT_EQ(0, fsconfig_call(fsfd, kFsconfigSetString, "lowerdir", lower.c_str(), 0))
        << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, fsconfig_call(fsfd, kFsconfigCreate, nullptr, nullptr, 0));
    EXPECT_EQ(EINVAL, errno);
    close(fsfd);
}

TEST_F(NewMountApiTest, OverlayLowerOnlyValidatesIgnoredWorkdir) {
    ASSERT_EQ(0, make_dir(src())) << strerror(errno);
    ASSERT_EQ(0, make_dir(dst())) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", src().c_str(), "tmpfs", 0, nullptr)) << strerror(errno);
    const std::string lower1 = src() + "/lower1";
    const std::string lower2 = src() + "/lower2";
    ASSERT_EQ(0, make_dir(lower1)) << strerror(errno);
    ASSERT_EQ(0, make_dir(lower2)) << strerror(errno);
    const std::string options = "lowerdir=" + lower1 + ":" + lower2 +
                                ",workdir=" + src() + "/missing";
    errno = 0;
    EXPECT_EQ(-1, mount("overlay", dst().c_str(), "overlay", 0, options.c_str()));
    EXPECT_EQ(ENOENT, errno);
}

TEST_F(NewMountApiTest, OverlayLowerOnlyRejectsNondirectoryWorkdir) {
    ASSERT_EQ(0, make_dir(src())) << strerror(errno);
    ASSERT_EQ(0, make_dir(dst())) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", src().c_str(), "tmpfs", 0, nullptr)) << strerror(errno);
    const std::string lower1 = src() + "/lower1";
    const std::string lower2 = src() + "/lower2";
    const std::string work = src() + "/file";
    ASSERT_EQ(0, make_dir(lower1)) << strerror(errno);
    ASSERT_EQ(0, make_dir(lower2)) << strerror(errno);
    ASSERT_EQ(0, write_marker(work)) << strerror(errno);

    const std::string options = "lowerdir=" + lower1 + ":" + lower2 + ",workdir=" + work;
    errno = 0;
    EXPECT_EQ(-1, mount("overlay", dst().c_str(), "overlay", 0, options.c_str()));
    EXPECT_EQ(EINVAL, errno);
    const int fsfd = fsopen_type("overlay", kFsopenCloexec);
    ASSERT_GE(fsfd, 0) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, fsconfig_call(fsfd, kFsconfigSetString, "workdir", work.c_str(), 0));
    EXPECT_EQ(EINVAL, errno);
    close(fsfd);
}

TEST_F(NewMountApiTest, OverlayLowerOnlyRejectsReadonlyWorkdirMount) {
    ASSERT_EQ(0, make_dir(src())) << strerror(errno);
    ASSERT_EQ(0, make_dir(dst())) << strerror(errno);
    ASSERT_EQ(0, mount("tmpfs", src().c_str(), "tmpfs", 0, nullptr)) << strerror(errno);
    const std::string lower1 = src() + "/lower1";
    const std::string lower2 = src() + "/lower2";
    const std::string work = src() + "/work";
    ASSERT_EQ(0, make_dir(lower1)) << strerror(errno);
    ASSERT_EQ(0, make_dir(lower2)) << strerror(errno);
    ASSERT_EQ(0, make_dir(work)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, src().c_str(), nullptr, MS_REMOUNT | MS_RDONLY, nullptr))
        << strerror(errno);

    const std::string options = "lowerdir=" + lower1 + ":" + lower2 + ",workdir=" + work;
    errno = 0;
    EXPECT_EQ(-1, mount("overlay", dst().c_str(), "overlay", 0, options.c_str()));
    EXPECT_EQ(EINVAL, errno);
    const int fsfd = fsopen_type("overlay", kFsopenCloexec);
    ASSERT_GE(fsfd, 0) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, fsconfig_call(fsfd, kFsconfigSetString, "workdir", work.c_str(), 0));
    EXPECT_EQ(EINVAL, errno);
    close(fsfd);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
