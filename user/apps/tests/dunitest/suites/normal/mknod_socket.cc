#include <errno.h>
#include <fcntl.h>
#include <gtest/gtest.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

#include <string>

namespace {

class TempDir {
  public:
    explicit TempDir(const char *prefix) {
        char tmpl[128];
        snprintf(tmpl, sizeof(tmpl), "/tmp/%s-%d-XXXXXX", prefix, getpid());
        char *created = mkdtemp(tmpl);
        if (created != nullptr) {
            path_ = created;
        }
    }

    ~TempDir() {
        if (!path_.empty()) {
            std::string node = path_ + "/sock";
            unlink(node.c_str());
            rmdir(path_.c_str());
        }
    }

    const std::string &path() const { return path_; }

  private:
    std::string path_;
};

class FdGuard {
  public:
    explicit FdGuard(int fd) : fd_(fd) {}
    ~FdGuard() {
        if (fd_ >= 0) {
            close(fd_);
        }
    }

    int get() const { return fd_; }

  private:
    int fd_;
};

} // namespace

TEST(MknodSocket, CreatesDisconnectedPathnameSocketNode) {
    TempDir dir("dunitest-mknod-socket");
    ASSERT_FALSE(dir.path().empty()) << "mkdtemp failed: " << strerror(errno);

    std::string socket_path = dir.path() + "/sock";
    ASSERT_EQ(0, mknod(socket_path.c_str(), S_IFSOCK | 0600, 0))
        << "mknod(S_IFSOCK) failed: errno=" << errno << " (" << strerror(errno) << ")";

    struct stat st {};
    ASSERT_EQ(0, lstat(socket_path.c_str(), &st)) << strerror(errno);
    EXPECT_TRUE(S_ISSOCK(st.st_mode));
    EXPECT_EQ(static_cast<mode_t>(0600), st.st_mode & 0777);

    ASSERT_EQ(-1, open(socket_path.c_str(), O_RDONLY));
    EXPECT_EQ(ENXIO, errno);

    FdGuard sock(socket(AF_UNIX, SOCK_SEQPACKET, 0));
    ASSERT_GE(sock.get(), 0) << "socket(AF_UNIX, SOCK_SEQPACKET) failed: " << strerror(errno);

    struct sockaddr_un addr {};
    addr.sun_family = AF_UNIX;
    snprintf(addr.sun_path, sizeof(addr.sun_path), "%s", socket_path.c_str());

    ASSERT_EQ(-1, connect(sock.get(), reinterpret_cast<struct sockaddr *>(&addr), sizeof(addr)));
    EXPECT_EQ(ECONNREFUSED, errno);
}

int main(int argc, char **argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
