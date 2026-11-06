// Unix socket inode metadata and sockfs semantics.

#include <gtest/gtest.h>

#include <errno.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/types.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>

namespace {

constexpr unsigned long kSockfsMagic = 0x534f434b;
constexpr unsigned long kSockfsNameMax = 255;
// musl does not expose ST_VALID; value from Linux include/linux/statfs.h.
constexpr unsigned long kStatFsValid = 0x0020;
constexpr unsigned kSocketPermissionMask = 07777;

struct Fd {
    int fd;
    explicit Fd(int value) : fd(value) {}
    ~Fd() {
        if (fd >= 0) close(fd);
    }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
};

sockaddr_un PathAddress(const std::string& path) {
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    snprintf(addr.sun_path, sizeof(addr.sun_path), "%s", path.c_str());
    return addr;
}

std::string UniquePath(const char* tag) {
    char path[108];
    snprintf(path, sizeof(path), "/tmp/dunitest-%s-%d", tag, getpid());
    return path;
}

struct AbstractAddress {
    sockaddr_un addr{};
    socklen_t len;

    explicit AbstractAddress(int suffix) {
        addr.sun_family = AF_UNIX;
        const int written = snprintf(addr.sun_path + 1, sizeof(addr.sun_path) - 1,
                                     "dunitest-stat-%d-%d", getpid(), suffix);
        len = offsetof(sockaddr_un, sun_path) + 1 + written;
    }
};

class ScopedUmask {
public:
    explicit ScopedUmask(mode_t mask) : previous_(umask(mask)) {}
    ~ScopedUmask() { umask(previous_); }
    ScopedUmask(const ScopedUmask&) = delete;
    ScopedUmask& operator=(const ScopedUmask&) = delete;

private:
    mode_t previous_;
};

unsigned PermissionBits(mode_t mode) {
    return static_cast<unsigned>(mode) & kSocketPermissionMask;
}

void ExpectUnixSocketStat(int fd) {
    struct stat st = {};
    ASSERT_EQ(0, fstat(fd, &st)) << strerror(errno);
    EXPECT_TRUE(S_ISSOCK(st.st_mode));
    EXPECT_EQ(0777U, PermissionBits(st.st_mode));
    EXPECT_EQ(static_cast<unsigned>(geteuid()), st.st_uid);
    EXPECT_EQ(static_cast<unsigned>(getegid()), st.st_gid);
    EXPECT_NE(0U, st.st_ino);
    // sockfs inodes carry the pseudo filesystem block size, not the 512-byte
    // fallback used by block devices.
    EXPECT_EQ(static_cast<blksize_t>(getpagesize()), st.st_blksize);
}

}  // namespace

TEST(UnixSocketStat, AnonymousSocketsStartWithLinuxDefaults) {
    for (int type : {SOCK_STREAM, SOCK_DGRAM}) {
        SCOPED_TRACE(type == SOCK_STREAM ? "SOCK_STREAM" : "SOCK_DGRAM");
        Fd fd(socket(AF_UNIX, type, 0));
        ASSERT_GE(fd.fd, 0) << strerror(errno);
        ExpectUnixSocketStat(fd.fd);
    }
}

TEST(UnixSocketStat, FchmodUpdatesModeAndCtime) {
    Fd fd(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(fd.fd, 0) << strerror(errno);

    struct stat before = {};
    ASSERT_EQ(0, fstat(fd.fd, &before)) << strerror(errno);
    ASSERT_EQ(0, fchmod(fd.fd, 0765)) << strerror(errno);
    struct stat st = {};
    ASSERT_EQ(0, fstat(fd.fd, &st)) << strerror(errno);
    EXPECT_EQ(static_cast<mode_t>(S_IFSOCK), st.st_mode & S_IFMT);
    EXPECT_EQ(0765U, PermissionBits(st.st_mode));
    EXPECT_NE(0, st.st_ctime);
    // chmod updates mode and ctime only; Linux leaves access/modify times
    // untouched.
    EXPECT_EQ(before.st_atime, st.st_atime);
    EXPECT_EQ(before.st_mtime, st.st_mtime);
}

TEST(UnixSocketStat, FstatfsReportsSockfs) {
    Fd fd(socket(AF_UNIX, SOCK_DGRAM, 0));
    ASSERT_GE(fd.fd, 0) << strerror(errno);

    struct statfs st = {};
    ASSERT_EQ(0, fstatfs(fd.fd, &st)) << strerror(errno);
    EXPECT_EQ(kSockfsMagic, static_cast<unsigned long>(st.f_type));
    EXPECT_EQ(static_cast<unsigned long>(getpagesize()), static_cast<unsigned long>(st.f_bsize));
    EXPECT_EQ(kSockfsNameMax, static_cast<unsigned long>(st.f_namelen));
    EXPECT_EQ(static_cast<unsigned long>(st.f_bsize), static_cast<unsigned long>(st.f_frsize));
    // sockfs is not mounted, so the only flag Linux reports is ST_VALID.
    EXPECT_EQ(kStatFsValid, static_cast<unsigned long>(st.f_flags));
}

TEST(UnixSocketStat, PathBindAppliesSocketModeAndUmask) {
    const std::string path = UniquePath("stream-bind");
    unlink(path.c_str());
    Fd fd(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(fd.fd, 0) << strerror(errno);
    ASSERT_EQ(0, fchmod(fd.fd, 0765)) << strerror(errno);
    ScopedUmask mask(0123);
    const sockaddr_un addr = PathAddress(path);
    ASSERT_EQ(0, bind(fd.fd, reinterpret_cast<const sockaddr*>(&addr), sizeof(addr)))
        << strerror(errno);

    struct stat st = {};
    ASSERT_EQ(0, stat(path.c_str(), &st)) << strerror(errno);
    EXPECT_TRUE(S_ISSOCK(st.st_mode));
    EXPECT_EQ(0644U, PermissionBits(st.st_mode));
    EXPECT_NE(0, st.st_atime);
    EXPECT_EQ(st.st_atime, st.st_mtime);
    EXPECT_EQ(st.st_atime, st.st_ctime);
    EXPECT_EQ(0, unlink(path.c_str())) << strerror(errno);
}

TEST(UnixSocketStat, DatagramPathBindUsesCurrentSocketMode) {
    const std::string path = UniquePath("dgram-bind");
    unlink(path.c_str());
    Fd fd(socket(AF_UNIX, SOCK_DGRAM, 0));
    ASSERT_GE(fd.fd, 0) << strerror(errno);
    ASSERT_EQ(0, fchmod(fd.fd, 0766)) << strerror(errno);
    ScopedUmask mask(0022);
    const sockaddr_un addr = PathAddress(path);
    ASSERT_EQ(0, bind(fd.fd, reinterpret_cast<const sockaddr*>(&addr), sizeof(addr)))
        << strerror(errno);

    struct stat st = {};
    ASSERT_EQ(0, stat(path.c_str(), &st)) << strerror(errno);
    EXPECT_TRUE(S_ISSOCK(st.st_mode));
    EXPECT_EQ(0744U, PermissionBits(st.st_mode));
    EXPECT_EQ(0, unlink(path.c_str())) << strerror(errno);
}

TEST(UnixSocketStat, AcceptedSocketIsOwnedByAcceptor) {
    if (geteuid() != 0) GTEST_SKIP() << "requires root to change euid";
    const AbstractAddress address(1);
    Fd listener(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(listener.fd, 0) << strerror(errno);
    ASSERT_EQ(0, bind(listener.fd, reinterpret_cast<const sockaddr*>(&address.addr), address.len))
        << strerror(errno);
    ASSERT_EQ(0, listen(listener.fd, 1)) << strerror(errno);

    int ready[2];
    ASSERT_EQ(0, pipe(ready)) << strerror(errno);
    const pid_t client_pid = fork();
    ASSERT_GE(client_pid, 0) << strerror(errno);
    if (client_pid == 0) {
        close(ready[0]);
        close(listener.fd);
        Fd client(socket(AF_UNIX, SOCK_STREAM, 0));
        const bool connected =
            client.fd >= 0 && seteuid(12345) == 0 &&
            connect(client.fd, reinterpret_cast<const sockaddr*>(&address.addr), address.len) == 0 &&
            seteuid(0) == 0;
        const char status = connected ? 'Y' : 'N';
        const ssize_t written = write(ready[1], &status, 1);
        _exit(connected && written == 1 ? 0 : 1);
    }

    close(ready[1]);
    char status = 0;
    ASSERT_EQ(1, read(ready[0], &status, 1)) << strerror(errno);
    close(ready[0]);
    if (status != 'Y') {
        int wait_status = 0;
        waitpid(client_pid, &wait_status, 0);
        FAIL() << "client failed to connect after switching euid";
    }

    Fd accepted(accept(listener.fd, nullptr, nullptr));
    ASSERT_GE(accepted.fd, 0) << strerror(errno);
    // Linux allocates the accepted inode in the accepting task, so the
    // connecting task's fsuid/fsgid must not leak into this descriptor.
    ExpectUnixSocketStat(accepted.fd);

    int wait_status = 0;
    ASSERT_EQ(client_pid, waitpid(client_pid, &wait_status, 0)) << strerror(errno);
    EXPECT_TRUE(WIFEXITED(wait_status) && WEXITSTATUS(wait_status) == 0);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
