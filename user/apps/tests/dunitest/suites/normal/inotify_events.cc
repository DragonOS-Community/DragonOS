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
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/wait.h>
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

int first_event_index(const std::vector<Ev> &evs, uint32_t bit) {
    for (size_t i = 0; i < evs.size(); i++) {
        if (evs[i].mask & bit) return static_cast<int>(i);
    }
    return -1;
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

TEST(InotifyNamespaceEvents, MknodAndMknodatDeliverCreate) {
    const std::string dir = "/tmp/dunitest_inotify_mknod";
    ASSERT_EQ(mkdir(dir.c_str(), 0777), 0) << strerror(errno);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, dir.c_str(), IN_CREATE), 0);

    const std::string first = dir + "/direct_fifo";
    ASSERT_EQ(syscall(SYS_mknod, first.c_str(), S_IFIFO | 0600, 0), 0) << strerror(errno);

    int dfd = open(dir.c_str(), O_RDONLY | O_DIRECTORY);
    ASSERT_GE(dfd, 0);
    ASSERT_EQ(syscall(SYS_mknodat, dfd, "at_fifo", S_IFIFO | 0600, 0), 0) << strerror(errno);

    auto evs = drain_events(ifd);
    EXPECT_TRUE(saw(evs, IN_CREATE, "direct_fifo"));
    EXPECT_TRUE(saw(evs, IN_CREATE, "at_fifo"));

    close(dfd);
    close(ifd);
    unlink(first.c_str());
    unlink((dir + "/at_fifo").c_str());
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

TEST(InotifyAttribEvent, UtimesDeliversAttrib) {
    const std::string path = "/tmp/dunitest_inotify_utimes";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, path.c_str(), IN_ATTRIB), 0);

    struct timeval times[2] = {{.tv_sec = 100, .tv_usec = 0}, {.tv_sec = 200, .tv_usec = 0}};
    ASSERT_EQ(utimes(path.c_str(), times), 0) << strerror(errno);
    EXPECT_TRUE(saw_self(drain_events(ifd), IN_ATTRIB));

    close(ifd);
    unlink(path.c_str());
}

TEST(InotifyAttribEvent, ChownNoopOnlyNotifiesWhenSpecialBitsChange) {
    const std::string path = "/tmp/dunitest_inotify_chown_noop";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, path.c_str(), IN_ATTRIB), 0);

    ASSERT_EQ(chown(path.c_str(), static_cast<uid_t>(-1), static_cast<gid_t>(-1)), 0);
    EXPECT_FALSE(saw_self(drain_events(ifd), IN_ATTRIB));

    // Raw syscall registers may contain non-zero high bits; uid_t/gid_t are
    // still 32-bit and Linux truncates them before interpreting -1.
    constexpr unsigned long kHighBitsNoChange = 0x1ffffffffUL;
    ASSERT_EQ(syscall(SYS_chown, path.c_str(), kHighBitsNoChange, kHighBitsNoChange), 0);
    EXPECT_FALSE(saw_self(drain_events(ifd), IN_ATTRIB));

    ASSERT_EQ(chmod(path.c_str(), 04755), 0);
    (void)drain_events(ifd);
    ASSERT_EQ(chown(path.c_str(), static_cast<uid_t>(-1), static_cast<gid_t>(-1)), 0);
    auto evs = drain_events(ifd);
    EXPECT_TRUE(saw_self(evs, IN_ATTRIB));
    struct stat st {};
    ASSERT_EQ(stat(path.c_str(), &st), 0);
    EXPECT_EQ(st.st_mode & S_ISUID, 0U);

    close(ifd);
    unlink(path.c_str());
}

TEST(InotifyAttribEvent, ChownFollowsSymlinksAndRejectsInvalidFlags) {
    const std::string target = "/tmp/dunitest_inotify_chown_target";
    const std::string link_path = "/tmp/dunitest_inotify_chown_link";
    const std::string loop_a = "/tmp/dunitest_inotify_chown_loop_a";
    const std::string loop_b = "/tmp/dunitest_inotify_chown_loop_b";
    int fd = open(target.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);
    ASSERT_EQ(symlink(target.c_str(), link_path.c_str()), 0);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, target.c_str(), IN_ATTRIB), 0);
    ASSERT_EQ(chown(link_path.c_str(), 0, static_cast<gid_t>(-1)), 0);
    EXPECT_TRUE(saw_self(drain_events(ifd), IN_ATTRIB));

    errno = 0;
    EXPECT_EQ(syscall(SYS_fchownat, AT_FDCWD, target.c_str(), -1, -1, 0x80000000U), -1);
    EXPECT_EQ(errno, EINVAL);

    ASSERT_EQ(symlink(loop_b.c_str(), loop_a.c_str()), 0);
    ASSERT_EQ(symlink(loop_a.c_str(), loop_b.c_str()), 0);
    errno = 0;
    EXPECT_EQ(chown(loop_a.c_str(), 0, static_cast<gid_t>(-1)), -1);
    EXPECT_EQ(errno, ELOOP);

    close(ifd);
    unlink(loop_a.c_str());
    unlink(loop_b.c_str());
    unlink(link_path.c_str());
    unlink(target.c_str());
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

TEST(InotifySelfEvents, OpenUnlinkDefersDeleteSelfUntilClose) {
    const std::string path = "/tmp/dunitest_inotify_delself_open";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, path.c_str(), IN_ATTRIB | IN_MODIFY | IN_DELETE_SELF), 0);

    ASSERT_EQ(unlink(path.c_str()), 0);
    ASSERT_EQ(write(fd, "x", 1), 1);
    auto before_close = drain_events(ifd);
    EXPECT_TRUE(saw_self(before_close, IN_ATTRIB));
    EXPECT_TRUE(saw_self(before_close, IN_MODIFY));
    EXPECT_FALSE(saw_self(before_close, IN_DELETE_SELF));
    EXPECT_FALSE(saw_self(before_close, IN_IGNORED));

    ASSERT_EQ(close(fd), 0);
    auto after_close = drain_events(ifd);
    EXPECT_TRUE(saw_self(after_close, IN_DELETE_SELF));
    EXPECT_TRUE(saw_self(after_close, IN_IGNORED));
    close(ifd);
}

TEST(InotifySelfEvents, HardLinkCreatedBeforeWatchKeepsWatchAlive) {
    const std::string first = "/tmp/dunitest_inotify_link_watch_a";
    const std::string second = "/tmp/dunitest_inotify_link_watch_b";
    int fd = open(first.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);
    ASSERT_EQ(link(first.c_str(), second.c_str()), 0);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, first.c_str(), IN_DELETE_SELF), 0);

    ASSERT_EQ(unlink(first.c_str()), 0);
    auto after_first = drain_events(ifd);
    EXPECT_FALSE(saw_self(after_first, IN_DELETE_SELF));
    EXPECT_FALSE(saw_self(after_first, IN_IGNORED));

    ASSERT_EQ(unlink(second.c_str()), 0);
    auto after_second = drain_events(ifd);
    EXPECT_TRUE(saw_self(after_second, IN_DELETE_SELF));
    EXPECT_TRUE(saw_self(after_second, IN_IGNORED));
    close(ifd);
}

TEST(InotifySelfEvents, WatchUnlinkedOpenFileThroughProcFd) {
    const std::string path = "/tmp/dunitest_inotify_procfd_unlinked";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    ASSERT_EQ(unlink(path.c_str()), 0);

    char procfd[64];
    snprintf(procfd, sizeof(procfd), "/proc/self/fd/%d", fd);
    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, procfd, IN_DELETE_SELF), 0)
        << "watching unlinked proc fd: " << strerror(errno);

    ASSERT_EQ(close(fd), 0);
    auto evs = drain_events(ifd);
    EXPECT_TRUE(saw_self(evs, IN_DELETE_SELF));
    EXPECT_TRUE(saw_self(evs, IN_IGNORED));
    close(ifd);
}

TEST(InotifyExcludeUnlinked, FtruncateRemainsADentryEvent) {
    const std::string path = "/tmp/dunitest_inotify_excl_ftruncate";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, path.c_str(), IN_MODIFY | IN_EXCL_UNLINK), 0);

    ASSERT_EQ(unlink(path.c_str()), 0);
    ASSERT_EQ(ftruncate(fd, 4096), 0);
    auto evs = drain_events(ifd);
    EXPECT_TRUE(saw_self(evs, IN_MODIFY));

    close(fd);
    close(ifd);
}

TEST(InotifyExcludeUnlinked, PathEventsAreSuppressedForDirectAndParentMarks) {
    const std::string dir = "/tmp/dunitest_inotify_excl_path";
    const std::string path = dir + "/file";
    mkdir(dir.c_str(), 0777);
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);

    int direct_ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    int parent_ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(direct_ifd, 0);
    ASSERT_GE(parent_ifd, 0);
    ASSERT_GE(inotify_add_watch(direct_ifd, path.c_str(), IN_MODIFY | IN_EXCL_UNLINK), 0);
    ASSERT_GE(inotify_add_watch(parent_ifd, dir.c_str(), IN_MODIFY | IN_EXCL_UNLINK), 0);

    ASSERT_EQ(unlink(path.c_str()), 0);
    ASSERT_EQ(write(fd, "x", 1), 1);
    EXPECT_FALSE(saw_self(drain_events(direct_ifd), IN_MODIFY));
    EXPECT_FALSE(saw(drain_events(parent_ifd), IN_MODIFY, "file"));

    // ftruncate is a dentry-data event on Linux, not a path-data event, so
    // IN_EXCL_UNLINK does not suppress it for either mark.
    ASSERT_EQ(ftruncate(fd, 2), 0);
    EXPECT_TRUE(saw_self(drain_events(direct_ifd), IN_MODIFY));
    EXPECT_TRUE(saw(drain_events(parent_ifd), IN_MODIFY, "file"));

    close(fd);
    close(direct_ifd);
    close(parent_ifd);
    rmdir(dir.c_str());
}

TEST(InotifyRenameEvents, SameInodeAliasIsNoOp) {
    const std::string first = "/tmp/dunitest_inotify_alias_a";
    const std::string second = "/tmp/dunitest_inotify_alias_b";
    int fd = open(first.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);
    ASSERT_EQ(link(first.c_str(), second.c_str()), 0);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, "/tmp", IN_MOVED_FROM | IN_MOVED_TO), 0);
    ASSERT_EQ(rename(first.c_str(), second.c_str()), 0);
    auto evs = drain_events(ifd);
    EXPECT_FALSE(saw(evs, IN_MOVED_FROM, "dunitest_inotify_alias_a"));
    EXPECT_FALSE(saw(evs, IN_MOVED_TO, "dunitest_inotify_alias_b"));

    close(ifd);
    unlink(first.c_str());
    unlink(second.c_str());
}

TEST(InotifyRenameEvents, NoOpAndNoReplacePrecedeDirectoryWriteChecks) {
    const std::string dir = "/tmp/dunitest_rename_readonly_alias";
    const std::string first = dir + "/a";
    const std::string second = dir + "/b";
    mkdir(dir.c_str(), 0755);
    int fd = open(first.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);
    ASSERT_EQ(link(first.c_str(), second.c_str()), 0);
    ASSERT_EQ(chmod(dir.c_str(), 0555), 0);

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (setgid(65534) != 0 || setuid(65534) != 0) _exit(10);
        if (rename(first.c_str(), second.c_str()) != 0) _exit(11);
        errno = 0;
        constexpr unsigned int kRenameNoReplace = 1;
        if (syscall(SYS_renameat2, AT_FDCWD, first.c_str(), AT_FDCWD, second.c_str(),
                    kRenameNoReplace) != -1 ||
            errno != EEXIST)
            _exit(12);
        _exit(0);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    EXPECT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);

    chmod(dir.c_str(), 0755);
    unlink(first.c_str());
    unlink(second.c_str());
    rmdir(dir.c_str());
}

TEST(InotifyRenameEvents, NoReplaceExistingTargetPrecedesAncestorTrap) {
    const std::string root = "/tmp/dunitest_rename_noreplace_trap";
    const std::string source = root + "/source";
    const std::string child = source + "/child";
    const std::string target = child + "/existing";
    mkdir(root.c_str(), 0755);
    mkdir(source.c_str(), 0755);
    mkdir(child.c_str(), 0755);
    int fd = open(target.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);

    constexpr unsigned int kRenameNoReplace = 1;
    errno = 0;
    EXPECT_EQ(syscall(SYS_renameat2, AT_FDCWD, source.c_str(), AT_FDCWD, target.c_str(),
                      kRenameNoReplace),
              -1);
    EXPECT_EQ(errno, EEXIST);

    unlink(target.c_str());
    rmdir(child.c_str());
    rmdir(source.c_str());
    rmdir(root.c_str());
}

TEST(InotifyRenameEvents, SameInodeAcrossBindMountsReturnsExdev) {
    const std::string root = "/tmp/dunitest_rename_bind_exdev";
    const std::string source = root + "/source";
    const std::string alias = root + "/alias";
    const std::string source_file = source + "/file";
    const std::string alias_file = alias + "/file";
    mkdir(root.c_str(), 0777);
    mkdir(source.c_str(), 0777);
    mkdir(alias.c_str(), 0777);
    int fd = open(source_file.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);
    ASSERT_EQ(mount(source.c_str(), alias.c_str(), nullptr, MS_BIND, nullptr), 0)
        << strerror(errno);

    errno = 0;
    EXPECT_EQ(rename(source_file.c_str(), alias_file.c_str()), -1);
    EXPECT_EQ(errno, EXDEV);

    EXPECT_EQ(umount(alias.c_str()), 0) << strerror(errno);
    unlink(source_file.c_str());
    rmdir(alias.c_str());
    rmdir(source.c_str());
    rmdir(root.c_str());
}

TEST(InotifyRenameEvents, LinuxMergesIdenticalMoveEventsIgnoringCookie) {
    const std::string root = "/tmp/dunitest_inotify_cookie_merge";
    const std::string watched = root + "/watched";
    const std::string outside = root + "/outside";
    const std::string watched_file = watched + "/file";
    const std::string outside_file = outside + "/file";
    mkdir(root.c_str(), 0777);
    mkdir(watched.c_str(), 0777);
    mkdir(outside.c_str(), 0777);
    int fd = open(watched_file.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, watched.c_str(), IN_MOVED_FROM), 0);
    ASSERT_EQ(rename(watched_file.c_str(), outside_file.c_str()), 0);
    ASSERT_EQ(rename(outside_file.c_str(), watched_file.c_str()), 0);
    ASSERT_EQ(rename(watched_file.c_str(), outside_file.c_str()), 0);

    auto evs = drain_events(ifd);
    size_t moved_from_count = 0;
    for (const auto &event : evs) {
        if ((event.mask & IN_MOVED_FROM) && event.name == "file") moved_from_count++;
    }
    EXPECT_EQ(moved_from_count, 1U);

    close(ifd);
    unlink(outside_file.c_str());
    rmdir(outside.c_str());
    rmdir(watched.c_str());
    rmdir(root.c_str());
}

TEST(InotifyOpenEvents, OPathProducesNoOpenOrClose) {
    const std::string path = "/tmp/dunitest_inotify_opath";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);
    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, path.c_str(), IN_OPEN | IN_CLOSE), 0);

    int pathfd = open(path.c_str(), O_PATH);
    ASSERT_GE(pathfd, 0);
    close(pathfd);
    auto evs = drain_events(ifd);
    EXPECT_FALSE(saw_self(evs, IN_OPEN));
    EXPECT_FALSE(saw_self(evs, IN_CLOSE_WRITE));
    EXPECT_FALSE(saw_self(evs, IN_CLOSE_NOWRITE));

    close(ifd);
    unlink(path.c_str());
}

TEST(InotifyOpenEvents, OTruncDeliversOpenBeforeModify) {
    const std::string path = "/tmp/dunitest_inotify_otrunc_order";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    ASSERT_EQ(write(fd, "payload", 7), 7);
    close(fd);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, path.c_str(), IN_OPEN | IN_MODIFY), 0);
    fd = open(path.c_str(), O_WRONLY | O_TRUNC);
    ASSERT_GE(fd, 0);
    close(fd);

    auto evs = drain_events(ifd);
    int open_index = first_event_index(evs, IN_OPEN);
    int modify_index = first_event_index(evs, IN_MODIFY);
    ASSERT_GE(open_index, 0);
    ASSERT_GE(modify_index, 0);
    EXPECT_LT(open_index, modify_index);

    close(ifd);
    unlink(path.c_str());
}

TEST(InotifyOpenEvents, NewlyCreatedOTruncFileHasNoModifyEvent) {
    const std::string dir = "/tmp/dunitest_inotify_otrunc_create";
    const std::string path = dir + "/file";
    mkdir(dir.c_str(), 0777);
    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, dir.c_str(), IN_CREATE | IN_OPEN | IN_MODIFY), 0);

    int fd = open(path.c_str(), O_CREAT | O_WRONLY | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);
    auto evs = drain_events(ifd);
    EXPECT_TRUE(saw(evs, IN_CREATE, "file"));
    EXPECT_TRUE(saw(evs, IN_OPEN, "file"));
    EXPECT_FALSE(saw(evs, IN_MODIFY, "file"));

    close(ifd);
    unlink(path.c_str());
    rmdir(dir.c_str());
}

TEST(InotifyOpenEvents, ExecDeliversOpenAndClose) {
    char executable[4096];
    ssize_t length = readlink("/proc/self/exe", executable, sizeof(executable) - 1);
    ASSERT_GT(length, 0);
    executable[length] = '\0';

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, executable, IN_OPEN | IN_CLOSE_NOWRITE), 0);

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        execl(executable, executable, "--gtest_filter=NoSuchExecProbe.*", nullptr);
        _exit(127);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(WEXITSTATUS(status), 0);

    auto evs = drain_events(ifd);
    int open_index = first_event_index(evs, IN_OPEN);
    int close_index = first_event_index(evs, IN_CLOSE_NOWRITE);
    ASSERT_GE(open_index, 0);
    ASSERT_GE(close_index, 0);
    EXPECT_LT(open_index, close_index);
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
