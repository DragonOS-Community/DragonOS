// Unix socket inode metadata and sockfs semantics.

#include <gtest/gtest.h>

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/un.h>
#include <unistd.h>

#include <string>

namespace {

constexpr unsigned long kSockfsMagic = 0x534f434b;
constexpr unsigned long kSockfsNameMax = 255;
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

    ASSERT_EQ(0, fchmod(fd.fd, 0765)) << strerror(errno);
    struct stat st = {};
    ASSERT_EQ(0, fstat(fd.fd, &st)) << strerror(errno);
    EXPECT_EQ(static_cast<mode_t>(S_IFSOCK), st.st_mode & S_IFMT);
    EXPECT_EQ(0765U, PermissionBits(st.st_mode));
    EXPECT_NE(0, st.st_ctime);
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

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
