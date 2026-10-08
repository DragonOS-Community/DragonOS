#include <gtest/gtest.h>
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/fsuid.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <sys/sysmacros.h>
#include <unistd.h>
#include <string>

namespace {
class TmpfsIdentityTest : public ::testing::Test {
protected:
    void SetUp() override {
        ASSERT_EQ(0, unshare(CLONE_NEWNS));
        ASSERT_EQ(0, mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr));
        char name[] = "/tmp/tmpfs_identity_XXXXXX";
        char* path = mkdtemp(name);
        ASSERT_NE(nullptr, path);
        path_ = path;
    }
    void TearDown() override {
        setfsuid(0);
        setfsgid(0);
        if (!path_.empty()) {
            umount2(path_.c_str(), MNT_DETACH);
            rmdir(path_.c_str());
        }
    }
    std::string path_;
};

TEST_F(TmpfsIdentityTest, LegacyDefaultRootIsSticky) {
    ASSERT_EQ(0, mount("tmpfs", path_.c_str(), "tmpfs", 0, nullptr));
    struct stat st {};
    ASSERT_EQ(0, stat(path_.c_str(), &st));
    EXPECT_EQ(01777u, st.st_mode & 07777);
}

TEST_F(TmpfsIdentityTest, ExplicitRootIdentity) {
    ASSERT_EQ(0, mount("tmpfs", path_.c_str(), "tmpfs", 0,
                       "uid=1234,gid=2345,mode=0750"));
    struct stat st {};
    ASSERT_EQ(0, stat(path_.c_str(), &st));
    EXPECT_EQ(1234u, st.st_uid);
    EXPECT_EQ(2345u, st.st_gid);
    EXPECT_EQ(0750u, st.st_mode & 07777);
}

TEST_F(TmpfsIdentityTest, FsopenRetainsCreatorFsIds) {
    setfsuid(1234);
    setfsgid(2345);
    int fd = syscall(SYS_fsopen, "tmpfs", 0);
    setfsuid(0);
    setfsgid(0);
    ASSERT_GE(fd, 0);
    int result = syscall(SYS_fsconfig, fd, 6, nullptr, nullptr, 0);
    if (result != 0) { close(fd); FAIL() << "CREATE errno=" << errno; }
    int tree = syscall(SYS_fsmount, fd, 0, 0);
    close(fd);
    ASSERT_GE(tree, 0);
    struct stat st {};
    result = fstat(tree, &st);
    close(tree);
    ASSERT_EQ(0, result);
    EXPECT_EQ(1234u, st.st_uid);
    EXPECT_EQ(2345u, st.st_gid);
    EXPECT_EQ(01777u, st.st_mode & 07777);
}

TEST_F(TmpfsIdentityTest, RemountDoesNotChangeRootIdentityOrPermissions) {
    ASSERT_EQ(0, mount("tmpfs", path_.c_str(), "tmpfs", 0, "uid=1234,gid=2345,mode=0750"));
    ASSERT_EQ(0, mount(nullptr, path_.c_str(), nullptr, MS_REMOUNT,
                       "uid=3456,gid=4567,mode=0700,size=4m"));
    int fd = syscall(SYS_fspick, AT_FDCWD, path_.c_str(), 0);
    ASSERT_GE(fd, 0);
    EXPECT_EQ(0, syscall(SYS_fsconfig, fd, 1, "mode", "0777", 0));
    EXPECT_EQ(0, syscall(SYS_fsconfig, fd, 1, "uid", "5678", 0));
    EXPECT_EQ(0, syscall(SYS_fsconfig, fd, 7, nullptr, nullptr, 0));
    close(fd);
    struct stat st {};
    ASSERT_EQ(0, stat(path_.c_str(), &st));
    EXPECT_EQ(1234u, st.st_uid);
    EXPECT_EQ(2345u, st.st_gid);
    EXPECT_EQ(0750u, st.st_mode & 07777);
}

TEST_F(TmpfsIdentityTest, NumericIdFormatsAndInvalidParametersDoNotPoisonContext) {
    int fd = syscall(SYS_fsopen, "tmpfs", 0);
    ASSERT_GE(fd, 0);
    for (const char* bad : {"08", "-1", "4294967295", "4294967296", "0x", "++1", "0x+1", "+0x+1", " 1", "1 ", "1\n\n"}) {
        EXPECT_EQ(-1, syscall(SYS_fsconfig, fd, 1, "uid", bad, 0));
        EXPECT_EQ(EINVAL, errno);
    }
    EXPECT_EQ(0, syscall(SYS_fsconfig, fd, 1, "uid", "010", 0));
    EXPECT_EQ(0, syscall(SYS_fsconfig, fd, 1, "gid", "+0x10", 0));
    EXPECT_EQ(0, syscall(SYS_fsconfig, fd, 6, nullptr, nullptr, 0));
    int tree = syscall(SYS_fsmount, fd, 0, 0);
    close(fd);
    ASSERT_GE(tree, 0);
    struct stat st {};
    EXPECT_EQ(0, fstat(tree, &st));
    close(tree);
    EXPECT_EQ(8u, st.st_uid);
    EXPECT_EQ(16u, st.st_gid);
}

TEST_F(TmpfsIdentityTest, LegacyIdentityOptionsKeepNumericValidation) {
    for (const char* bad : {"uid=1 ", "uid=1\n\n", "uid=++1", "gid=0x+1", "uid= 1"}) {
        EXPECT_EQ(-1, mount("tmpfs", path_.c_str(), "tmpfs", 0, bad));
        EXPECT_EQ(EINVAL, errno);
    }
    ASSERT_EQ(0, mount("tmpfs", path_.c_str(), "tmpfs", 0, "uid=010,gid=+0x10"));
    struct stat st {};
    ASSERT_EQ(0, stat(path_.c_str(), &st));
    EXPECT_EQ(8u, st.st_uid);
    EXPECT_EQ(16u, st.st_gid);
}

struct WorkerResult { int line = 0; int error = 0; };
#define WORKER_CHECK(expr) do { if (!(expr)) return WorkerResult{__LINE__, errno}; } while (0)

int write_text(const std::string& path, const char* text) {
    int fd = open(path.c_str(), O_WRONLY);
    if (fd < 0) return -1;
    ssize_t count = write(fd, text, strlen(text));
    close(fd);
    return count == static_cast<ssize_t>(strlen(text)) ? 0 : -1;
}

// Parent installs mappings while the child is blocked. Assertions are sent
// back explicitly; child gtest failures must never disappear with _exit().
template <typename Action>
void in_mapped_namespace(Action action,
                         const char* uid_map = "0 0 1\n42 1234 1\n",
                         const char* gid_map = "0 0 1\n43 2345 1\n") {
    int ready[2], release[2];
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(release));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(ready[0]); close(release[1]);
        int error = unshare(CLONE_NEWUSER | CLONE_NEWNS) == 0 ? 0 : errno;
        if (write(ready[1], &error, sizeof(error)) != sizeof(error)) _exit(2);
        char ack;
        if (read(release[0], &ack, 1) != 1 || ack != 'y') _exit(3);
        WorkerResult result = error ? WorkerResult{__LINE__, error} : action();
        if (write(ready[1], &result, sizeof(result)) != sizeof(result)) _exit(4);
        _exit(0);
    }
    close(ready[1]); close(release[0]);
    int error = EIO;
    EXPECT_EQ(static_cast<ssize_t>(sizeof(error)), read(ready[0], &error, sizeof(error)));
    const std::string base = "/proc/" + std::to_string(child);
    bool mapped = error == 0;
    if (mapped) mapped = write_text(base + "/uid_map", uid_map) == 0;
    if (mapped) mapped = write_text(base + "/setgroups", "deny\n") == 0;
    if (mapped) mapped = write_text(base + "/gid_map", gid_map) == 0;
    char ack = mapped ? 'y' : 'n';
    EXPECT_EQ(1, write(release[1], &ack, 1));
    WorkerResult result;
    if (mapped) EXPECT_EQ(static_cast<ssize_t>(sizeof(result)), read(ready[0], &result, sizeof(result)));
    close(ready[0]); close(release[1]);
    int status = 0;
    EXPECT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(mapped) << "userns/maps prerequisite errno=" << error;
    EXPECT_TRUE(WIFEXITED(status) && WEXITSTATUS(status) == 0) << status;
    EXPECT_EQ(0, result.line) << "worker line=" << result.line << " errno=" << result.error;
}

TEST_F(TmpfsIdentityTest, ChildNamespaceLegacyAndNewApiAgree) {
    in_mapped_namespace([&] {
        WORKER_CHECK(mount("tmpfs", path_.c_str(), "tmpfs", 0, "uid=42,gid=43") == 0);
        struct stat st {};
        WORKER_CHECK(stat(path_.c_str(), &st) == 0);
        WORKER_CHECK(st.st_uid == 42 && st.st_gid == 43 && (st.st_mode & 07777) == 01777);
        WORKER_CHECK(umount(path_.c_str()) == 0);
        int fd = syscall(SYS_fsopen, "tmpfs", 0);
        WORKER_CHECK(fd >= 0);
        WORKER_CHECK(syscall(SYS_fsconfig, fd, 1, "uid", "44", 0) == -1 && errno == EINVAL);
        WORKER_CHECK(syscall(SYS_fsconfig, fd, 1, "uid", "42", 0) == 0);
        WORKER_CHECK(syscall(SYS_fsconfig, fd, 1, "gid", "43", 0) == 0);
        WORKER_CHECK(syscall(SYS_fsconfig, fd, 6, nullptr, nullptr, 0) == 0);
        int tree = syscall(SYS_fsmount, fd, 0, 0);
        close(fd);
        WORKER_CHECK(tree >= 0);
        WORKER_CHECK(fstat(tree, &st) == 0);
        close(tree);
        WORKER_CHECK(st.st_uid == 42 && st.st_gid == 43);
        return WorkerResult{};
    });
}

TEST_F(TmpfsIdentityTest, ChildOwnerDeviceAccessCannotBeEnabledByRemount) {
    in_mapped_namespace([&] {
      for (bool modern : {false, true}) {
        if (modern) {
            int fd = syscall(SYS_fsopen, "tmpfs", 0);
            WORKER_CHECK(fd >= 0);
            WORKER_CHECK(syscall(SYS_fsconfig, fd, 6, nullptr, nullptr, 0) == 0);
            int tree = syscall(SYS_fsmount, fd, 0, 0);
            close(fd);
            WORKER_CHECK(tree >= 0);
            int result = syscall(SYS_move_mount, tree, "", AT_FDCWD, path_.c_str(), 4);
            close(tree);
            WORKER_CHECK(result == 0);
        } else {
            WORKER_CHECK(mount("tmpfs", path_.c_str(), "tmpfs", 0, nullptr) == 0);
        }
        const std::string node = path_ + "/null";
        // Linux permits the 0:0 whiteout node without initial CAP_MKNOD;
        // opening it must still fail with EACCES, not device lookup ENODEV.
        WORKER_CHECK(mknod(node.c_str(), S_IFCHR | 0666, makedev(0, 0)) == 0);
        WORKER_CHECK(mount(nullptr, path_.c_str(), nullptr, MS_REMOUNT, nullptr) == 0);
        int fd = open(node.c_str(), O_PATH);
        WORKER_CHECK(fd >= 0);
        close(fd);
        WORKER_CHECK(open(node.c_str(), O_RDWR) == -1 && errno == EACCES);
        const std::string alias = path_ + "/alias";
        WORKER_CHECK(mkdir(alias.c_str(), 0700) == 0);
        WORKER_CHECK(mount(path_.c_str(), alias.c_str(), nullptr, MS_BIND, nullptr) == 0);
        WORKER_CHECK(mount(nullptr, alias.c_str(), nullptr, MS_BIND | MS_REMOUNT, nullptr) == 0);
        struct { uint64_t set, clear, propagation, userns_fd; } attr{0, 4, 0, 0};
        WORKER_CHECK(syscall(SYS_mount_setattr, AT_FDCWD, alias.c_str(), 0, &attr, sizeof(attr)) == 0);
        WORKER_CHECK(open((alias + "/null").c_str(), O_RDWR) == -1 && errno == EACCES);
        WORKER_CHECK(umount(alias.c_str()) == 0);
        WORKER_CHECK(rmdir(alias.c_str()) == 0);
        WORKER_CHECK(unlink(node.c_str()) == 0);
        WORKER_CHECK(umount(path_.c_str()) == 0);
      }
        return WorkerResult{};
    });
}

TEST_F(TmpfsIdentityTest, ChildOwnerRejectsUnmappedChownIds) {
    in_mapped_namespace([&] {
        WORKER_CHECK(mount("tmpfs", path_.c_str(), "tmpfs", 0, nullptr) == 0);
        WORKER_CHECK(chown(path_.c_str(), 44, 43) == -1 && errno == EINVAL);
        WORKER_CHECK(chown(path_.c_str(), 42, 43) == 0);
        WORKER_CHECK(umount(path_.c_str()) == 0);
        return WorkerResult{};
    });
}

TEST_F(TmpfsIdentityTest, ParametersFreezeSetterNamespaceButCreationChecksCurrentCapability) {
    int fd = syscall(SYS_fsopen, "tmpfs", 0);
    ASSERT_GE(fd, 0);
    in_mapped_namespace([&] {
        WORKER_CHECK(syscall(SYS_fsconfig, fd, 1, "uid", "42", 0) == 0);
        WORKER_CHECK(syscall(SYS_fsconfig, fd, 1, "gid", "43", 0) == 0);
        // The child can configure this inherited fd but cannot authorize a
        // superblock owned by the initial namespace. Failure is retryable.
        WORKER_CHECK(syscall(SYS_fsconfig, fd, 6, nullptr, nullptr, 0) == -1 && errno == EPERM);
        return WorkerResult{};
    });
    EXPECT_EQ(0, syscall(SYS_fsconfig, fd, 6, nullptr, nullptr, 0));
    int tree = syscall(SYS_fsmount, fd, 0, 0);
    close(fd);
    ASSERT_GE(tree, 0);
    struct stat st {};
    EXPECT_EQ(0, fstat(tree, &st));
    close(tree);
    EXPECT_EQ(1234u, st.st_uid);
    EXPECT_EQ(2345u, st.st_gid);
}

TEST_F(TmpfsIdentityTest, ReconfigureDoesNotReinterpretSetterIds) {
    ASSERT_EQ(0, mount("tmpfs", path_.c_str(), "tmpfs", 0, nullptr));
    int fd = syscall(SYS_fspick, AT_FDCWD, path_.c_str(), 0);
    ASSERT_GE(fd, 0);
    in_mapped_namespace([&] {
        WORKER_CHECK(syscall(SYS_fsconfig, fd, 1, "uid", "42", 0) == 0);
        WORKER_CHECK(syscall(SYS_fsconfig, fd, 1, "gid", "43", 0) == 0);
        return WorkerResult{};
    });
    EXPECT_EQ(0, syscall(SYS_fsconfig, fd, 7, nullptr, nullptr, 0));
    close(fd);
    struct stat st {};
    ASSERT_EQ(0, stat(path_.c_str(), &st));
    EXPECT_EQ(0u, st.st_uid);
    EXPECT_EQ(0u, st.st_gid);
}

TEST_F(TmpfsIdentityTest, ChildNamespaceDefaultUsesFsopenIdentity) {
    in_mapped_namespace([&] {
        setfsuid(42); setfsgid(43);
        int fd = syscall(SYS_fsopen, "tmpfs", 0);
        setfsuid(0); setfsgid(0);
        WORKER_CHECK(fd >= 0);
        WORKER_CHECK(syscall(SYS_fsconfig, fd, 6, nullptr, nullptr, 0) == 0);
        int tree = syscall(SYS_fsmount, fd, 0, 0);
        close(fd);
        WORKER_CHECK(tree >= 0);
        struct stat st {};
        WORKER_CHECK(fstat(tree, &st) == 0);
        close(tree);
        WORKER_CHECK(st.st_uid == 42 && st.st_gid == 43);
        WORKER_CHECK(mount("none", path_.c_str(), "ext4", 0, nullptr) == -1 && errno == EPERM);
        return WorkerResult{};
    });
}

TEST_F(TmpfsIdentityTest, UnmappedDefaultRootAllowsAccessButNotNewInodeCreation) {
    in_mapped_namespace([&] {
        WORKER_CHECK(mount("tmpfs", path_.c_str(), "tmpfs", 0, nullptr) == 0);
        // Linux keeps global creator IDs even when they have no mapping in
        // the fs owner namespace. Existing-inode access is not creation.
        WORKER_CHECK(access(path_.c_str(), W_OK) == 0);
        const std::string child = path_ + "/child";
        WORKER_CHECK(mkdir(child.c_str(), 0700) == -1 && errno == EOVERFLOW);
        WORKER_CHECK(open(child.c_str(), O_CREAT | O_RDWR, 0600) == -1 && errno == EOVERFLOW);
        // vfs_tmpfile does not call may_create; nop-idmapped anonymous
        // creation preserves the valid global creator IDs even here.
        int fd = open(path_.c_str(), O_TMPFILE | O_RDWR, 0600);
        WORKER_CHECK(fd >= 0);
        WORKER_CHECK(write(fd, "x", 1) == 1);
        close(fd);
        WORKER_CHECK(umount(path_.c_str()) == 0);
        return WorkerResult{};
    }, "42 1234 1\n", "43 2345 1\n");
}

TEST_F(TmpfsIdentityTest, InitialNamespaceCanChownAndWriteChildOwnedAnonymousFile) {
    int sockets[2];
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_DGRAM, 0, sockets));
    in_mapped_namespace([&] {
        WORKER_CHECK(mount("tmpfs", path_.c_str(), "tmpfs", 0, nullptr) == 0);
        int fd = open(path_.c_str(), O_TMPFILE | O_RDWR, 0600);
        WORKER_CHECK(fd >= 0);
        char byte = 'x';
        iovec iov{&byte, 1};
        alignas(cmsghdr) char control[CMSG_SPACE(sizeof(int))] = {};
        msghdr message{};
        message.msg_iov = &iov;
        message.msg_iovlen = 1;
        message.msg_control = control;
        message.msg_controllen = sizeof(control);
        cmsghdr* cmsg = CMSG_FIRSTHDR(&message);
        cmsg->cmsg_level = SOL_SOCKET;
        cmsg->cmsg_type = SCM_RIGHTS;
        cmsg->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(cmsg), &fd, sizeof(fd));
        WORKER_CHECK(sendmsg(sockets[1], &message, 0) == 1);
        close(fd);
        WORKER_CHECK(umount2(path_.c_str(), MNT_DETACH) == 0);
        return WorkerResult{};
    });
    char byte;
    iovec iov{&byte, 1};
    alignas(cmsghdr) char control[CMSG_SPACE(sizeof(int))] = {};
    msghdr message{};
    message.msg_iov = &iov;
    message.msg_iovlen = 1;
    message.msg_control = control;
    message.msg_controllen = sizeof(control);
    ssize_t count = recvmsg(sockets[0], &message, MSG_DONTWAIT | MSG_CMSG_CLOEXEC);
    close(sockets[0]); close(sockets[1]);
    ASSERT_EQ(1, count);
    cmsghdr* cmsg = CMSG_FIRSTHDR(&message);
    ASSERT_NE(nullptr, cmsg);
    ASSERT_EQ(SCM_RIGHTS, cmsg->cmsg_type);
    int fd;
    memcpy(&fd, CMSG_DATA(cmsg), sizeof(fd));
    EXPECT_EQ(0, fchown(fd, 6666, 7777));
    EXPECT_EQ(1, write(fd, "x", 1));
    struct stat st {};
    EXPECT_EQ(0, fstat(fd, &st));
    close(fd);
    EXPECT_EQ(6666u, st.st_uid);
    EXPECT_EQ(7777u, st.st_gid);
}
} // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
