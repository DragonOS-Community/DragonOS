#include <gtest/gtest.h>
#include <atomic>
#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <sched.h>
#include <string>
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <thread>
#include <unistd.h>
#include <vector>

namespace {
class Fd {
 public:
    explicit Fd(int fd) : fd_(fd) {}
    ~Fd() { if (fd_ >= 0) close(fd_); }
    int get() const { return fd_; }
 private:
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int fd_;
};
std::string FdPath(int fd) {
    char buffer[8192];
    const std::string link = "/proc/self/fd/" + std::to_string(fd);
    ssize_t n = readlink(link.c_str(), buffer, sizeof(buffer));
    return n < 0 ? std::string() : std::string(buffer, n);
}
class ReadlinkPathSnapshot : public testing::Test {
 protected:
    void SetUp() override {
        char pattern[] = "/tmp/readlink-snapshot-XXXXXX";
        char* result = mkdtemp(pattern);
        ASSERT_NE(nullptr, result);
        base_ = result;
        dirs_.push_back(base_);
    }
    void TearDown() override {
        for (auto i = files_.rbegin(); i != files_.rend(); ++i) unlink(i->c_str());
        for (auto i = dirs_.rbegin(); i != dirs_.rend(); ++i) rmdir(i->c_str());
    }
    bool Dir(const std::string& path) {
        if (mkdir(path.c_str(), 0700) != 0) return false;
        dirs_.push_back(path);
        return true;
    }
    int File(const std::string& path) {
        files_.push_back(path);
        return open(path.c_str(), O_CREAT | O_RDWR, 0600);
    }
    void CheckRetainedFd(bool detach_before_send);
    std::string base_;
    std::vector<std::string> dirs_, files_;
};
TEST_F(ReadlinkPathSnapshot, DeepPathAndDeletedSuffix) {
    std::string path = base_;
    for (int depth = 0; depth <= 64; ++depth) {
        if (depth) { path += "/d"; ASSERT_TRUE(Dir(path)); }
        if (depth != 0 && depth != 8 && depth != 32 && depth != 64) continue;
        Fd fd(File(path + "/leaf"));
        ASSERT_GE(fd.get(), 0);
        EXPECT_EQ(path + "/leaf", FdPath(fd.get()));
        ASSERT_EQ(0, unlink((path + "/leaf").c_str()));
        EXPECT_EQ(path + "/leaf (deleted)", FdPath(fd.get()));
    }
}
TEST_F(ReadlinkPathSnapshot, RenameReturnsCompleteOldOrNewPath) {
    ASSERT_TRUE(Dir(base_ + "/left"));
    ASSERT_TRUE(Dir(base_ + "/right"));
    const std::string old_dir = base_ + "/left/old";
    const std::string new_dir = base_ + "/right/new";
    ASSERT_TRUE(Dir(old_dir));
    Fd fd(File(old_dir + "/leaf"));
    ASSERT_GE(fd.get(), 0);
    std::atomic<bool> start{false};
    std::atomic<int> error{0};
    std::thread writer([&] {
        while (!start.load()) std::this_thread::yield();
        for (int i = 0; i < 300; ++i) {
            if (rename(old_dir.c_str(), new_dir.c_str()) != 0) { error = errno; break; }
            if (rename(new_dir.c_str(), old_dir.c_str()) != 0) { error = errno; break; }
        }
    });
    start = true;
    bool complete = true;
    for (int i = 0; i < 600; ++i) {
        const std::string path = FdPath(fd.get());
        if (path != old_dir + "/leaf" && path != new_dir + "/leaf") complete = false;
    }
    writer.join();
    EXPECT_EQ(0, error.load());
    EXPECT_TRUE(complete);
    // Also clean the alternative name if a failed rename left it there.
    unlink((new_dir + "/leaf").c_str());
    rmdir(new_dir.c_str());
}

// All mount mutations are confined to a child namespace, including on failure.
TEST_F(ReadlinkPathSnapshot, BindAndStackedMountPreserveRetainedPaths) {
    ASSERT_TRUE(Dir(base_ + "/source"));
    ASSERT_TRUE(Dir(base_ + "/view"));
    Fd original(File(base_ + "/source/leaf"));
    ASSERT_GE(original.get(), 0);
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (unshare(CLONE_NEWNS) != 0) _exit(errno == EPERM ? 77 : 10);
        if (mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr)) _exit(11);
        const std::string view = base_ + "/view";
        if (mount((base_ + "/source").c_str(), view.c_str(), nullptr, MS_BIND, nullptr)) _exit(12);
        Fd retained(open((view + "/leaf").c_str(), O_RDONLY));
        if (retained.get() < 0 || FdPath(retained.get()) != view + "/leaf") _exit(13);
        if (mount("none", view.c_str(), "tmpfs", 0, nullptr)) _exit(14);
        Fd top(open(view.c_str(), O_RDONLY | O_DIRECTORY));
        if (top.get() < 0 || FdPath(top.get()) != view) _exit(15);
        if (FdPath(retained.get()) != view + "/leaf") _exit(16);
        if (FdPath(original.get()) != base_ + "/source/leaf") _exit(17);
        _exit(0);
    }
    int status;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    if (WEXITSTATUS(status) == 77) GTEST_SKIP() << "mount namespace requires CAP_SYS_ADMIN";
    EXPECT_EQ(0, WEXITSTATUS(status));
}
void ReadlinkPathSnapshot::CheckRetainedFd(bool detach_before_send) {
    ASSERT_TRUE(Dir(base_ + "/mount"));
    int sockets[2];
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_DGRAM, 0, sockets));
    Fd receiver(sockets[0]), sender(sockets[1]);
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (unshare(CLONE_NEWNS) != 0) _exit(errno == EPERM ? 77 : 10);
        if (mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr)) _exit(11);
        if (mount("none", (base_ + "/mount").c_str(), "tmpfs", 0, nullptr)) _exit(12);
        int fd = open((base_ + "/mount/leaf").c_str(), O_CREAT | O_RDWR, 0600);
        if (fd < 0 || write(fd, "x", 1) != 1) _exit(13);
        if (detach_before_send && umount2((base_ + "/mount").c_str(), MNT_DETACH) != 0)
            _exit(15);
        char byte = 'x';
        iovec iov{&byte, 1};
        alignas(cmsghdr) char control[CMSG_SPACE(sizeof(int))] = {};
        msghdr msg{};
        msg.msg_iov = &iov; msg.msg_iovlen = 1;
        msg.msg_control = control; msg.msg_controllen = sizeof(control);
        cmsghdr* cmsg = CMSG_FIRSTHDR(&msg);
        cmsg->cmsg_level = SOL_SOCKET; cmsg->cmsg_type = SCM_RIGHTS;
        cmsg->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(cmsg), &fd, sizeof(fd));
        _exit(sendmsg(sender.get(), &msg, 0) == 1 ? 0 : 14);
    }
    int status;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    if (WEXITSTATUS(status) == 77) GTEST_SKIP() << "mount namespace requires CAP_SYS_ADMIN";
    ASSERT_EQ(0, WEXITSTATUS(status));
    char byte;
    iovec iov{&byte, 1};
    alignas(cmsghdr) char control[CMSG_SPACE(sizeof(int))] = {};
    msghdr msg{};
    msg.msg_iov = &iov; msg.msg_iovlen = 1;
    msg.msg_control = control; msg.msg_controllen = sizeof(control);
    ASSERT_EQ(1, recvmsg(receiver.get(), &msg, MSG_DONTWAIT));
    cmsghdr* cmsg = CMSG_FIRSTHDR(&msg);
    ASSERT_NE(nullptr, cmsg);
    ASSERT_EQ(SOL_SOCKET, cmsg->cmsg_level);
    ASSERT_EQ(SCM_RIGHTS, cmsg->cmsg_type);
    ASSERT_EQ(CMSG_LEN(sizeof(int)), cmsg->cmsg_len);
    int received;
    memcpy(&received, CMSG_DATA(cmsg), sizeof(received));
    Fd retained(received);
    EXPECT_EQ(1, pread(retained.get(), &byte, 1, 0));
    EXPECT_EQ('x', byte);
    // waitpid does not establish that deferred namespace retirement finished.
    // Explicit detach gives the path assertion a completed topology transition.
    if (detach_before_send) EXPECT_EQ("/leaf", FdPath(retained.get()));
}
TEST_F(ReadlinkPathSnapshot, RetainedFdRemainsReadableAfterNamespaceOwnerExits) {
    CheckRetainedFd(false);
}
TEST_F(ReadlinkPathSnapshot, RetainedDetachedFdPreservesPathAfterNamespaceOwnerExits) {
    CheckRetainedFd(true);
}
}  // namespace
int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
