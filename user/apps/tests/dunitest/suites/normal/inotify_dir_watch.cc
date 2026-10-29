// inotify_dir_watch.cc - inotify directory-watch child content events test (dunitest/gtest)
//
// Regression coverage for the inotify directory-watch fix (issue B):
// watching a directory must deliver child *content* events
// (IN_MODIFY / IN_ACCESS / IN_OPEN / IN_CLOSE_WRITE / IN_CLOSE_NOWRITE) carrying the
// child name -- the dominant inotify use case (e.g. `inotifywait -m /dir`).
// Before the fix, directory watches only received namespace events (create/delete/move)
// and silently dropped all child content events.
//
// Runtime environment: DragonOS QEMU, /tmp is a writable tmpfs.
// Use GTEST_SKIP() if inotify syscalls are unavailable.

#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#include <cstdint>
#include <string>
#include <vector>

namespace {

struct Ev {
    uint32_t mask;
    std::string name;
};

// VFS notifications are queued before the triggering syscall returns. Drain
// the currently queued prefix and stop as soon as the nonblocking fd is empty.
std::vector<Ev> drain_events(int ifd) {
    std::vector<Ev> out;
    char buf[4096] __attribute__((aligned(8)));
    for (;;) {
        ssize_t n = read(ifd, buf, sizeof(buf));
        if (n < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK) {
                break;
            }
            break;
        }
        if (n == 0) break;
        for (char *p = buf; p + sizeof(struct inotify_event) <= buf + n;) {
            struct inotify_event *e = reinterpret_cast<struct inotify_event *>(p);
            out.push_back(Ev{e->mask, e->len ? std::string(e->name) : std::string()});
            p += sizeof(struct inotify_event) + e->len;
        }
    }
    return out;
}

bool saw(const std::vector<Ev> &evs, uint32_t bit, const std::string &name) {
    for (const auto &e : evs) {
        if ((e.mask & bit) && e.name == name) return true;
    }
    return false;
}

// Touch a regular file inside `dir` named `base`: create+write+close(write),
// then open(read)+read+close(read).
void exercise_child(const std::string &dir, const std::string &base) {
    std::string path = dir + "/" + base;
    int fd = open(path.c_str(), O_CREAT | O_WRONLY | O_TRUNC, 0644);
    ASSERT_GE(fd, 0) << "open create: " << strerror(errno);
    ASSERT_EQ(write(fd, "hello\n", 6), 6);
    ASSERT_EQ(close(fd), 0);
    int rfd = open(path.c_str(), O_RDONLY);
    ASSERT_GE(rfd, 0) << "open read: " << strerror(errno);
    char rb[16];
    ASSERT_GE(read(rfd, rb, sizeof(rb)), 0);
    ASSERT_EQ(close(rfd), 0);
}

}  // namespace

// Core regression: a directory watch receives child IN_MODIFY/IN_CREATE/IN_CLOSE_WRITE.
TEST(InotifyDirWatch, ChildContentEventsReachDirWatch) {
    const std::string dir = "/tmp/dunitest_inotify_dir";
    const std::string child = "f";
    mkdir(dir.c_str(), 0777);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0) << "inotify_init1: " << strerror(errno);

    int wd = inotify_add_watch(
        ifd, dir.c_str(),
        IN_CREATE | IN_MODIFY | IN_ACCESS | IN_OPEN | IN_CLOSE_WRITE | IN_CLOSE_NOWRITE);
    ASSERT_GE(wd, 0) << "inotify_add_watch: " << strerror(errno);

    exercise_child(dir, child);
    auto evs = drain_events(ifd);

    // Diagnostic: print what we actually saw.
    for (const auto &e : evs) {
        printf("  saw mask=0x%x name=\"%s\"\n", e.mask, e.name.c_str());
    }

    EXPECT_TRUE(saw(evs, IN_CREATE, child))
        << "dir watch missed child IN_CREATE";
    EXPECT_TRUE(saw(evs, IN_MODIFY, child))
        << "dir watch missed child IN_MODIFY (issue B regression)";
    EXPECT_TRUE(saw(evs, IN_CLOSE_WRITE, child))
        << "dir watch missed child IN_CLOSE_WRITE";

    inotify_rm_watch(ifd, wd);
    close(ifd);
    unlink((dir + "/" + child).c_str());
    rmdir(dir.c_str());
}

// Sanity (unchanged behavior): watching a file *itself* still receives IN_MODIFY.
TEST(InotifySelfWatch, ModifyReachesFileWatch) {
    const std::string path = "/tmp/dunitest_inotify_self";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    int wd = inotify_add_watch(ifd, path.c_str(), IN_MODIFY);
    ASSERT_GE(wd, 0);

    ASSERT_EQ(write(fd, "x", 1), 1);
    auto evs = drain_events(ifd);

    bool got = false;
    for (const auto &e : evs) {
        if (e.mask & IN_MODIFY) got = true;
    }
    EXPECT_TRUE(got) << "self watch missed IN_MODIFY";

    inotify_rm_watch(ifd, wd);
    close(ifd);
    close(fd);
    unlink(path.c_str());
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
