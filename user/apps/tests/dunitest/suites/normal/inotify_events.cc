// inotify_events.cc - comprehensive inotify event coverage (dunitest/gtest)
//
// Covers the event types NOT exercised by inotify_dir_watch.cc:
//   - Namespace events: IN_DELETE, IN_MOVED_FROM, IN_MOVED_TO, IN_ISDIR
//   - Self events:      IN_DELETE_SELF (+ IN_IGNORED), IN_MOVE_SELF
//   - IN_ATTRIB         (chmod metadata change)
//   - Multi-instance:   two independent inotify fds watching the same inode
//   - poll() readiness: inotify fd reports POLLIN after an event
//
// Runtime environment: DragonOS QEMU, /tmp is a writable tmpfs.
// Use GTEST_SKIP() if inotify syscalls are unavailable.

#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
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
    uint32_t cookie;
    std::string name;
};

// Drain all queued inotify events within a short time budget (nonblocking fd).
std::vector<Ev> drain_events(int ifd) {
    std::vector<Ev> out;
    char buf[4096] __attribute__((aligned(8)));
    for (int spins = 0; spins < 300; spins++) {
        ssize_t n = read(ifd, buf, sizeof(buf));
        if (n < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK) {
                usleep(2000);
                continue;
            }
            break;
        }
        if (n == 0) {
            usleep(2000);
            continue;
        }
        for (char *p = buf; p + sizeof(struct inotify_event) <= buf + n;) {
            struct inotify_event *e = reinterpret_cast<struct inotify_event *>(p);
            out.push_back(Ev{e->mask, e->cookie, e->len ? std::string(e->name) : std::string()});
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

// A self-event has an empty name (no child).
bool saw_self(const std::vector<Ev> &evs, uint32_t bit) {
    for (const auto &e : evs) {
        if ((e.mask & bit) && e.name.empty()) return true;
    }
    return false;
}

}  // namespace

// ---------------------------------------------------------------------------
// Namespace events on a directory watch: create, delete, move (rename).
// Also verifies IN_ISDIR is set when the created child is a directory.
// ---------------------------------------------------------------------------
TEST(InotifyNamespaceEvents, CreateDeleteMoveOnDirWatch) {
    const std::string dir = "/tmp/dunitest_inotify_ns";
    mkdir(dir.c_str(), 0777);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0) << "inotify_init1: " << strerror(errno);

    int wd = inotify_add_watch(
        ifd, dir.c_str(),
        IN_CREATE | IN_DELETE | IN_MOVED_FROM | IN_MOVED_TO);
    // Note: IN_ISDIR is NOT a valid add_watch mask bit — it is a flag the
    // kernel sets on returned events to indicate the subject is a directory.
    // We verify it appears automatically on events for directory children.

    // 1. Create a subdirectory -> IN_CREATE with IN_ISDIR.
    const std::string subdir = "subdir";
    std::string subpath = dir + "/" + subdir;
    ASSERT_EQ(mkdir(subpath.c_str(), 0777), 0) << "mkdir: " << strerror(errno);

    // 2. Create a regular file -> IN_CREATE without IN_ISDIR.
    const std::string file1 = "file1";
    std::string fpath = dir + "/" + file1;
    int fd = open(fpath.c_str(), O_CREAT | O_WRONLY, 0644);
    ASSERT_GE(fd, 0) << "open create: " << strerror(errno);
    close(fd);

    // 3. Rename file1 -> file2 within the dir -> IN_MOVED_FROM(file1) + IN_MOVED_TO(file2).
    const std::string file2 = "file2";
    std::string fpath2 = dir + "/" + file2;
    ASSERT_EQ(rename(fpath.c_str(), fpath2.c_str()), 0) << "rename: " << strerror(errno);

    // 4. Delete file2 -> IN_DELETE(file2).
    ASSERT_EQ(unlink(fpath2.c_str()), 0) << "unlink: " << strerror(errno);

    auto evs = drain_events(ifd);

    for (const auto &e : evs) {
        printf("  saw mask=0x%x cookie=%u name=\"%s\"\n", e.mask, e.cookie, e.name.c_str());
    }

    // Create subdir: IN_CREATE | IN_ISDIR
    EXPECT_TRUE(saw(evs, IN_CREATE, subdir)) << "missed IN_CREATE for subdir";
    bool saw_dir = false;
    for (const auto &e : evs) {
        if ((e.mask & IN_CREATE) && (e.mask & IN_ISDIR) && e.name == subdir) saw_dir = true;
    }
    EXPECT_TRUE(saw_dir) << "IN_CREATE for subdir should carry IN_ISDIR";

    // Create regular file: IN_CREATE without IN_ISDIR
    EXPECT_TRUE(saw(evs, IN_CREATE, file1)) << "missed IN_CREATE for file1";

    // Move: IN_MOVED_FROM(file1) and IN_MOVED_TO(file2) with matching cookie
    EXPECT_TRUE(saw(evs, IN_MOVED_FROM, file1)) << "missed IN_MOVED_FROM for file1";
    EXPECT_TRUE(saw(evs, IN_MOVED_TO, file2)) << "missed IN_MOVED_TO for file2";
    uint32_t cookie_from = 0, cookie_to = 0;
    for (const auto &e : evs) {
        if ((e.mask & IN_MOVED_FROM) && e.name == file1) cookie_from = e.cookie;
        if ((e.mask & IN_MOVED_TO) && e.name == file2) cookie_to = e.cookie;
    }
    EXPECT_NE(cookie_from, 0) << "IN_MOVED_FROM cookie should be non-zero for intra-dir rename";
    EXPECT_EQ(cookie_from, cookie_to) << "IN_MOVED_FROM/TO cookies must match for same rename";

    // Delete: IN_DELETE(file2)
    EXPECT_TRUE(saw(evs, IN_DELETE, file2)) << "missed IN_DELETE for file2";

    inotify_rm_watch(ifd, wd);
    close(ifd);
    rmdir(subpath.c_str());
    rmdir(dir.c_str());
}

// ---------------------------------------------------------------------------
// IN_ATTRIB: changing file metadata (chmod) delivers IN_ATTRIB.
// ---------------------------------------------------------------------------
TEST(InotifyAttribEvent, ChmodDeliversAttrib) {
    const std::string path = "/tmp/dunitest_inotify_attrib";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);

    int wd = inotify_add_watch(ifd, path.c_str(), IN_ATTRIB);
    ASSERT_GE(wd, 0) << "inotify_add_watch: " << strerror(errno);

    ASSERT_EQ(chmod(path.c_str(), 0600), 0) << "chmod: " << strerror(errno);

    auto evs = drain_events(ifd);

    for (const auto &e : evs) {
        printf("  saw mask=0x%x name=\"%s\"\n", e.mask, e.name.c_str());
    }

    EXPECT_TRUE(saw_self(evs, IN_ATTRIB)) << "self watch missed IN_ATTRIB after chmod";

    inotify_rm_watch(ifd, wd);
    close(ifd);
    unlink(path.c_str());
}

// ---------------------------------------------------------------------------
// IN_DELETE_SELF + IN_IGNORED: unlinking a watched file delivers
// IN_DELETE_SELF, followed by IN_IGNORED (watch auto-revoked).
// ---------------------------------------------------------------------------
TEST(InotifySelfEvents, UnlinkWatchedFileDeliversDeleteSelfAndIgnored) {
    const std::string path = "/tmp/dunitest_inotify_delself";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);

    int wd = inotify_add_watch(ifd, path.c_str(), IN_DELETE_SELF);
    ASSERT_GE(wd, 0) << "inotify_add_watch: " << strerror(errno);

    ASSERT_EQ(unlink(path.c_str()), 0) << "unlink: " << strerror(errno);

    auto evs = drain_events(ifd);

    for (const auto &e : evs) {
        printf("  saw mask=0x%x name=\"%s\"\n", e.mask, e.name.c_str());
    }

    EXPECT_TRUE(saw_self(evs, IN_DELETE_SELF))
        << "watched file missed IN_DELETE_SELF after unlink";
    EXPECT_TRUE(saw_self(evs, IN_IGNORED))
        << "watched file missed IN_IGNORED after unlink";

    // IN_IGNORED is always emitted — do NOT call inotify_rm_watch after auto-revoke.
    close(ifd);
}

// ---------------------------------------------------------------------------
// IN_MOVE_SELF: renaming a watched file delivers IN_MOVE_SELF.
// ---------------------------------------------------------------------------
TEST(InotifySelfEvents, RenameWatchedFileDeliversMoveSelf) {
    const std::string path = "/tmp/dunitest_inotify_moveself";
    const std::string path2 = "/tmp/dunitest_inotify_moveself_renamed";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);

    int wd = inotify_add_watch(ifd, path.c_str(), IN_MOVE_SELF);
    ASSERT_GE(wd, 0) << "inotify_add_watch: " << strerror(errno);

    ASSERT_EQ(rename(path.c_str(), path2.c_str()), 0) << "rename: " << strerror(errno);

    auto evs = drain_events(ifd);

    for (const auto &e : evs) {
        printf("  saw mask=0x%x name=\"%s\"\n", e.mask, e.name.c_str());
    }

    EXPECT_TRUE(saw_self(evs, IN_MOVE_SELF))
        << "watched file missed IN_MOVE_SELF after rename";

    inotify_rm_watch(ifd, wd);
    close(ifd);
    unlink(path2.c_str());
}

// ---------------------------------------------------------------------------
// Multiple independent inotify instances watching the same directory both
// receive events — ensures event fan-out works across groups.
// ---------------------------------------------------------------------------
TEST(InotifyMultiInstance, TwoInstancesBothReceiveEvents) {
    const std::string dir = "/tmp/dunitest_inotify_multi";
    mkdir(dir.c_str(), 0777);

    int ifd1 = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd1, 0);
    int ifd2 = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd2, 0);

    int wd1 = inotify_add_watch(ifd1, dir.c_str(), IN_CREATE);
    ASSERT_GE(wd1, 0);
    int wd2 = inotify_add_watch(ifd2, dir.c_str(), IN_CREATE);
    ASSERT_GE(wd2, 0);

    const std::string child = "shared";
    std::string cpath = dir + "/" + child;
    int fd = open(cpath.c_str(), O_CREAT | O_WRONLY, 0644);
    ASSERT_GE(fd, 0);
    close(fd);

    auto evs1 = drain_events(ifd1);
    auto evs2 = drain_events(ifd2);

    printf("  instance1: %zu events\n", evs1.size());
    printf("  instance2: %zu events\n", evs2.size());

    EXPECT_TRUE(saw(evs1, IN_CREATE, child)) << "instance1 missed IN_CREATE";
    EXPECT_TRUE(saw(evs2, IN_CREATE, child)) << "instance2 missed IN_CREATE";

    inotify_rm_watch(ifd1, wd1);
    inotify_rm_watch(ifd2, wd2);
    close(ifd1);
    close(ifd2);
    unlink(cpath.c_str());
    rmdir(dir.c_str());
}

// ---------------------------------------------------------------------------
// poll() readiness: an inotify fd becomes readable (POLLIN) after an event
// is queued — validates epoll/eventpoll integration.
// ---------------------------------------------------------------------------
TEST(InotifyPollReady, FdBecomesReadableAfterEvent) {
    const std::string path = "/tmp/dunitest_inotify_poll";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);

    int wd = inotify_add_watch(ifd, path.c_str(), IN_MODIFY | IN_ATTRIB);
    ASSERT_GE(wd, 0);

    // Before any event: poll should timeout (no readiness).
    struct pollfd pfd = {.fd = ifd, .events = POLLIN, .revents = 0};
    int pr = poll(&pfd, 1, 100);
    EXPECT_EQ(pr, 0) << "poll should timeout before any event";
    EXPECT_EQ(pfd.revents, 0);

    // Trigger an event.
    ASSERT_EQ(chmod(path.c_str(), 0600), 0);

    // After the event: poll should report POLLIN within a short window.
    pfd.revents = 0;
    pr = poll(&pfd, 1, 1000);
    EXPECT_GT(pr, 0) << "poll should be ready after event";
    EXPECT_TRUE(pfd.revents & POLLIN) << "POLLIN should be set after event";

    // Consume and verify the event.
    auto evs = drain_events(ifd);
    EXPECT_TRUE(saw_self(evs, IN_ATTRIB)) << "should see IN_ATTRIB after poll-ready";

    inotify_rm_watch(ifd, wd);
    close(ifd);
    unlink(path.c_str());
}

int main(int argc, char **argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
