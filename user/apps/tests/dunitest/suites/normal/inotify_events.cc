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
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/sendfile.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

#include <algorithm>
#include <cstdint>
#include <atomic>
#include <chrono>
#include <string>
#include <thread>
#include <vector>

namespace {

struct Ev {
    int wd;
    uint32_t mask;
    uint32_t cookie;
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
            out.push_back(
                Ev{e->wd, e->mask, e->cookie, e->len ? std::string(e->name) : std::string()});
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

void queue_exact_64k_self_events(int ifd, int fd) {
    ASSERT_GE(inotify_add_watch(ifd, ("/proc/self/fd/" + std::to_string(fd)).c_str(),
                                IN_ATTRIB | IN_MODIFY),
              0);
    constexpr int kPairs = 2048;
    const char byte = 'x';
    for (int i = 0; i < kPairs; ++i) {
        ASSERT_EQ(fchmod(fd, (i & 1) ? 0644 : 0600), 0) << strerror(errno);
        ASSERT_EQ(pwrite(fd, &byte, 1, 0), 1) << strerror(errno);
    }
}

int first_event_index(const std::vector<Ev> &evs, uint32_t bit) {
    for (size_t i = 0; i < evs.size(); i++) {
        if (evs[i].mask & bit) return static_cast<int>(i);
    }
    return -1;
}

int add_fd_watch(int ifd, int fd, uint32_t mask) {
    char procfd[64];
    snprintf(procfd, sizeof(procfd), "/proc/self/fd/%d", fd);
    return inotify_add_watch(ifd, procfd, mask);
}

int first_event_index(const std::vector<Ev> &evs, int wd, uint32_t bit) {
    for (size_t i = 0; i < evs.size(); i++) {
        if (evs[i].wd == wd && (evs[i].mask & bit)) return static_cast<int>(i);
    }
    return -1;
}

int event_count(const std::vector<Ev> &evs, int wd, uint32_t bit,
                const std::string &name) {
    int count = 0;
    for (const auto &event : evs) {
        if (event.wd == wd && (event.mask & bit) && event.name == name) ++count;
    }
    return count;
}

size_t inotify_record_size(const std::string &name) {
    if (name.empty()) return sizeof(struct inotify_event);
    const size_t alignment = sizeof(struct inotify_event);
    const size_t name_size = (name.size() + 1 + alignment - 1) & ~(alignment - 1);
    return sizeof(struct inotify_event) + name_size;
}

struct InotifyTestCleanup {
    std::string path;
    std::string dir;
    std::vector<std::string> extra_paths;
    std::vector<std::string> extra_dirs;
    std::vector<int> extra_wds;
    int fd = -1;
    int ifd = -1;
    int parent_wd = -1;
    int self_wd = -1;
    bool directory_created = false;
    bool file_created = false;

    ~InotifyTestCleanup() {
        if (ifd >= 0) {
            for (int wd : extra_wds) inotify_rm_watch(ifd, wd);
            if (parent_wd >= 0) inotify_rm_watch(ifd, parent_wd);
            if (self_wd >= 0) inotify_rm_watch(ifd, self_wd);
            close(ifd);
        }
        if (fd >= 0) close(fd);
        for (const auto &extra_path : extra_paths) unlink(extra_path.c_str());
        if (file_created) unlink(path.c_str());
        for (auto it = extra_dirs.rbegin(); it != extra_dirs.rend(); ++it)
            rmdir(it->c_str());
        if (directory_created) rmdir(dir.c_str());
    }
};

}  // namespace

TEST(InotifyFileEvents, LargeReadPublishesOneAccessPerWatch) {
    const std::string dir = "/tmp/dunitest_inotify_large_read";
    const std::string name = "input";
    const std::string path = dir + "/" + name;
    ASSERT_EQ(mkdir(dir.c_str(), 0700), 0) << strerror(errno);

    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0600);
    ASSERT_GE(fd, 0) << strerror(errno);
    constexpr size_t kSize = 200 * 1024;
    std::vector<char> data(kSize, 'x');
    ASSERT_EQ(write(fd, data.data(), data.size()), static_cast<ssize_t>(data.size()))
        << strerror(errno);
    ASSERT_EQ(lseek(fd, 0, SEEK_SET), 0);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0) << strerror(errno);
    int self_wd = inotify_add_watch(ifd, path.c_str(), IN_ACCESS);
    int parent_wd = inotify_add_watch(ifd, dir.c_str(), IN_ACCESS);
    ASSERT_GE(self_wd, 0) << strerror(errno);
    ASSERT_GE(parent_wd, 0) << strerror(errno);

    std::vector<char> output(kSize);
    ASSERT_EQ(read(fd, output.data(), output.size()), static_cast<ssize_t>(output.size()))
        << strerror(errno);
    auto events = drain_events(ifd);
    EXPECT_EQ(event_count(events, self_wd, IN_ACCESS, ""), 1);
    EXPECT_EQ(event_count(events, parent_wd, IN_ACCESS, name), 1);

    ASSERT_EQ(readv(fd, nullptr, 0), 0) << strerror(errno);
    events = drain_events(ifd);
    EXPECT_EQ(event_count(events, self_wd, IN_ACCESS, ""), 1);
    EXPECT_EQ(event_count(events, parent_wd, IN_ACCESS, name), 1);

    close(ifd);
    close(fd);
    ASSERT_EQ(unlink(path.c_str()), 0);
    ASSERT_EQ(rmdir(dir.c_str()), 0);
}

TEST(InotifyFileEvents, ReadvPublishesOneAccessPerWatch) {
    const std::string dir = "/tmp/dunitest_inotify_readv";
    const std::string name = "input";
    const std::string path = dir + "/" + name;
    ASSERT_EQ(mkdir(dir.c_str(), 0700), 0) << strerror(errno);

    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0600);
    ASSERT_GE(fd, 0) << strerror(errno);
    constexpr size_t kPartSize = 96 * 1024;
    std::vector<char> data(kPartSize * 2, 'x');
    ASSERT_EQ(write(fd, data.data(), data.size()), static_cast<ssize_t>(data.size()))
        << strerror(errno);
    ASSERT_EQ(lseek(fd, 0, SEEK_SET), 0);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0) << strerror(errno);
    int self_wd = inotify_add_watch(ifd, path.c_str(), IN_ACCESS);
    int parent_wd = inotify_add_watch(ifd, dir.c_str(), IN_ACCESS);
    ASSERT_GE(self_wd, 0) << strerror(errno);
    ASSERT_GE(parent_wd, 0) << strerror(errno);

    std::vector<char> first(kPartSize);
    std::vector<char> second(kPartSize);
    struct iovec iov[2] = {
        {.iov_base = first.data(), .iov_len = first.size()},
        {.iov_base = second.data(), .iov_len = second.size()},
    };
    ASSERT_EQ(readv(fd, iov, 2), static_cast<ssize_t>(data.size())) << strerror(errno);
    auto events = drain_events(ifd);
    EXPECT_EQ(event_count(events, self_wd, IN_ACCESS, ""), 1);
    EXPECT_EQ(event_count(events, parent_wd, IN_ACCESS, name), 1);

    close(ifd);
    close(fd);
    ASSERT_EQ(unlink(path.c_str()), 0);
    ASSERT_EQ(rmdir(dir.c_str()), 0);
}

TEST(InotifyFileEvents, LargePositionedIoPublishesOneEventPerWatch) {
    const std::string dir = "/tmp/dunitest_inotify_large_positioned_io";
    const std::string name = "file";
    const std::string path = dir + "/" + name;
    ASSERT_EQ(mkdir(dir.c_str(), 0700), 0) << strerror(errno);
    InotifyTestCleanup cleanup{path, dir};
    cleanup.directory_created = true;

    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0600);
    ASSERT_GE(fd, 0) << strerror(errno);
    cleanup.fd = fd;
    cleanup.file_created = true;
    constexpr size_t kSize = 200 * 1024;
    std::vector<char> input(kSize, 'x');
    std::vector<char> output(kSize);
    ASSERT_EQ(write(fd, input.data(), input.size()), static_cast<ssize_t>(input.size()))
        << strerror(errno);
    ASSERT_EQ(lseek(fd, 17, SEEK_SET), 17);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0) << strerror(errno);
    cleanup.ifd = ifd;
    int self_wd = inotify_add_watch(ifd, path.c_str(), IN_ACCESS | IN_MODIFY);
    int parent_wd = inotify_add_watch(ifd, dir.c_str(), IN_ACCESS | IN_MODIFY);
    ASSERT_GE(self_wd, 0) << strerror(errno);
    cleanup.self_wd = self_wd;
    ASSERT_GE(parent_wd, 0) << strerror(errno);
    cleanup.parent_wd = parent_wd;

    ASSERT_EQ(pread(fd, output.data(), output.size(), 0),
              static_cast<ssize_t>(output.size()))
        << strerror(errno);
    EXPECT_EQ(output, input);
    EXPECT_EQ(lseek(fd, 0, SEEK_CUR), 17);
    auto events = drain_events(ifd);
    EXPECT_EQ(event_count(events, self_wd, IN_ACCESS, ""), 1);
    EXPECT_EQ(event_count(events, parent_wd, IN_ACCESS, name), 1);

    auto expect_one_access_per_watch = [&]() {
        auto access_events = drain_events(ifd);
        EXPECT_EQ(event_count(access_events, self_wd, IN_ACCESS, ""), 1);
        EXPECT_EQ(event_count(access_events, parent_wd, IN_ACCESS, name), 1);
    };

    struct iovec zero_iov = {output.data(), 0};
    ASSERT_EQ(preadv(fd, &zero_iov, 1, 0), 0) << strerror(errno);
    expect_one_access_per_watch();

    // Linux accepts iovcnt==0 without touching the iovec pointer and still
    // publishes the successful vector-read ACCESS event.
    ASSERT_EQ(preadv(fd, reinterpret_cast<const struct iovec *>(1), 0, 0), 0)
        << strerror(errno);
    expect_one_access_per_watch();

    // preadv validates its signed offset before accessing a nonempty invalid
    // iovec, matching Linux's EINVAL-before-EFAULT error priority.
    errno = 0;
    ASSERT_EQ(syscall(SYS_preadv, fd,
                      reinterpret_cast<const struct iovec *>(1), 1, -1L),
              -1);
    EXPECT_EQ(errno, EINVAL);
    events = drain_events(ifd);
    EXPECT_EQ(event_count(events, self_wd, IN_ACCESS, ""), 0);
    EXPECT_EQ(event_count(events, parent_wd, IN_ACCESS, name), 0);

    // preadv2 applies the same priority to offsets below its -1 sentinel,
    // before both iovec import and unsupported-flag validation.
    errno = 0;
    ASSERT_EQ(syscall(SYS_preadv2, fd,
                      reinterpret_cast<const struct iovec *>(1), 1, -2L, -1L,
                      0L),
              -1);
    EXPECT_EQ(errno, EINVAL);
    errno = 0;
    ASSERT_EQ(syscall(SYS_preadv2, fd, &zero_iov, 1, -2L, -1L,
                      0x80000000L),
              -1);
    EXPECT_EQ(errno, EINVAL);
    events = drain_events(ifd);
    EXPECT_EQ(event_count(events, self_wd, IN_ACCESS, ""), 0);
    EXPECT_EQ(event_count(events, parent_wd, IN_ACCESS, name), 0);

    struct iovec eof_iov = {output.data(), 1};
    ASSERT_EQ(preadv(fd, &eof_iov, 1, static_cast<off_t>(kSize)), 0)
        << strerror(errno);
    expect_one_access_per_watch();

    // preadv2(offset=-1) uses and preserves the current file position at EOF,
    // but follows the same successful-zero vector notification contract.
    ASSERT_EQ(lseek(fd, static_cast<off_t>(kSize), SEEK_SET),
              static_cast<off_t>(kSize));
    ASSERT_EQ(syscall(SYS_preadv2, fd, &eof_iov, 1, -1L, -1L, 0L), 0)
        << strerror(errno);
    EXPECT_EQ(lseek(fd, 0, SEEK_CUR), static_cast<off_t>(kSize));
    expect_one_access_per_watch();

    // Scalar pread differs: a successful EOF result does not publish ACCESS.
    ASSERT_EQ(pread(fd, output.data(), 1, static_cast<off_t>(kSize)), 0)
        << strerror(errno);
    events = drain_events(ifd);
    EXPECT_EQ(event_count(events, self_wd, IN_ACCESS, ""), 0);
    EXPECT_EQ(event_count(events, parent_wd, IN_ACCESS, name), 0);

    ASSERT_EQ(lseek(fd, 17, SEEK_SET), 17);

    std::fill(input.begin(), input.end(), 'y');
    ASSERT_EQ(pwrite(fd, input.data(), input.size(), 0),
              static_cast<ssize_t>(input.size()))
        << strerror(errno);
    EXPECT_EQ(lseek(fd, 0, SEEK_CUR), 17);
    events = drain_events(ifd);
    EXPECT_EQ(event_count(events, self_wd, IN_MODIFY, ""), 1);
    EXPECT_EQ(event_count(events, parent_wd, IN_MODIFY, name), 1);
}

TEST(InotifyFileEvents, NegativeInterestCacheTracksWatchAndRename) {
    const std::string root = "/tmp/dunitest_inotify_interest_cache";
    const std::string cold = root + "/cold";
    const std::string watched = root + "/watched";
    const std::string name = "file";
    const std::string old_path = cold + "/" + name;
    const std::string new_path = watched + "/" + name;

    ASSERT_EQ(mkdir(root.c_str(), 0700), 0) << strerror(errno);
    InotifyTestCleanup cleanup{"", root};
    cleanup.directory_created = true;
    ASSERT_EQ(mkdir(cold.c_str(), 0700), 0) << strerror(errno);
    cleanup.extra_dirs.push_back(cold);
    ASSERT_EQ(mkdir(watched.c_str(), 0700), 0) << strerror(errno);
    cleanup.extra_dirs.push_back(watched);
    cleanup.extra_paths = {old_path, new_path};

    int fd = open(old_path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0600);
    ASSERT_GE(fd, 0) << strerror(errno);
    cleanup.fd = fd;
    ASSERT_EQ(write(fd, "a", 1), 1) << strerror(errno);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0) << strerror(errno);
    cleanup.ifd = ifd;
    cleanup.parent_wd = inotify_add_watch(ifd, root.c_str(), IN_MODIFY);
    ASSERT_GE(cleanup.parent_wd, 0) << strerror(errno);

    // The root watch enables this superblock's fsnotify path, but neither the
    // file nor its direct parent is watched. This I/O establishes a negative
    // interest cache for the file.
    char byte = 0;
    ASSERT_EQ(pread(fd, &byte, 1, 0), 1) << strerror(errno);
    EXPECT_TRUE(drain_events(ifd).empty());

    cleanup.self_wd = inotify_add_watch(ifd, old_path.c_str(), IN_MODIFY);
    ASSERT_GE(cleanup.self_wd, 0) << strerror(errno);
    ASSERT_EQ(pwrite(fd, "b", 1, 0), 1) << strerror(errno);
    auto events = drain_events(ifd);
    EXPECT_EQ(event_count(events, cleanup.self_wd, IN_MODIFY, ""), 1);

    ASSERT_EQ(inotify_rm_watch(ifd, cleanup.self_wd), 0) << strerror(errno);
    cleanup.self_wd = -1;
    drain_events(ifd);

    int watched_wd = inotify_add_watch(ifd, watched.c_str(), IN_MODIFY | IN_MOVED_TO);
    ASSERT_GE(watched_wd, 0) << strerror(errno);
    cleanup.extra_wds.push_back(watched_wd);

    // Cache the old, unwatched direct parent, then move the same dentry under
    // an already-watched parent. The topology commit must invalidate it.
    ASSERT_EQ(pwrite(fd, "c", 1, 0), 1) << strerror(errno);
    EXPECT_TRUE(drain_events(ifd).empty());
    ASSERT_EQ(rename(old_path.c_str(), new_path.c_str()), 0) << strerror(errno);
    drain_events(ifd);

    ASSERT_EQ(pwrite(fd, "d", 1, 0), 1) << strerror(errno);
    events = drain_events(ifd);
    EXPECT_EQ(event_count(events, watched_wd, IN_MODIFY, name), 1);
}

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

TEST(InotifyNamespaceEvents, ConcurrentMknodDeletePreservesCommitOrder) {
    const std::string dir = "/tmp/dunitest_inotify_mknod_order";
    ASSERT_EQ(mkdir(dir.c_str(), 0777), 0) << strerror(errno);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, dir.c_str(), IN_CREATE | IN_DELETE), 0);

    constexpr int kNodes = 64;
    std::atomic<int> worker_error{0};
    std::atomic<bool> stop{false};
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(5);
    std::thread remover([&]() {
        for (int i = 0; i < kNodes; ++i) {
            const std::string path = dir + "/node_" + std::to_string(i);
            struct stat st {};
            while (lstat(path.c_str(), &st) != 0) {
                if (errno != ENOENT) {
                    worker_error.store(errno);
                    return;
                }
                if (stop.load()) return;
                if (std::chrono::steady_clock::now() >= deadline) {
                    worker_error.store(ETIMEDOUT);
                    stop.store(true);
                    return;
                }
                sched_yield();
            }
            if (unlink(path.c_str()) != 0) {
                worker_error.store(errno);
                return;
            }
        }
    });

    for (int i = 0; i < kNodes; ++i) {
        const std::string path = dir + "/node_" + std::to_string(i);
        if (syscall(SYS_mknod, path.c_str(), S_IFIFO | 0600, 0) != 0) {
            worker_error.store(errno);
            stop.store(true);
            break;
        }
        struct stat st {};
        while (lstat(path.c_str(), &st) == 0) {
            if (worker_error.load() != 0 ||
                std::chrono::steady_clock::now() >= deadline) {
                if (worker_error.load() == 0) worker_error.store(ETIMEDOUT);
                stop.store(true);
                break;
            }
            sched_yield();
        }
        if (stop.load()) break;
        if (errno != ENOENT) {
            worker_error.store(errno);
            stop.store(true);
            break;
        }
    }
    remover.join();
    ASSERT_EQ(worker_error.load(), 0) << strerror(worker_error.load());

    auto evs = drain_events(ifd);
    for (int i = 0; i < kNodes; ++i) {
        const std::string name = "node_" + std::to_string(i);
        int created = -1;
        int deleted = -1;
        for (size_t j = 0; j < evs.size(); ++j) {
            if (evs[j].name != name) continue;
            if (created < 0 && (evs[j].mask & IN_CREATE)) created = static_cast<int>(j);
            if (deleted < 0 && (evs[j].mask & IN_DELETE)) deleted = static_cast<int>(j);
        }
        ASSERT_GE(created, 0) << "missing IN_CREATE for " << name;
        ASSERT_GE(deleted, 0) << "missing IN_DELETE for " << name;
        EXPECT_LT(created, deleted) << "namespace events reordered for " << name;
    }

    close(ifd);
    rmdir(dir.c_str());
}

TEST(InotifyNamespaceEvents, ConcurrentOpenCreateDeletePreservesCommitOrder) {
    const std::string dir = "/tmp/dunitest_inotify_open_create_order";
    ASSERT_EQ(mkdir(dir.c_str(), 0777), 0) << strerror(errno);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, dir.c_str(), IN_CREATE | IN_DELETE), 0);

    constexpr int kFiles = 64;
    std::atomic<int> worker_error{0};
    std::atomic<bool> stop{false};
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(5);
    std::thread remover([&]() {
        for (int i = 0; i < kFiles; ++i) {
            const std::string path = dir + "/file_" + std::to_string(i);
            struct stat st {};
            while (lstat(path.c_str(), &st) != 0) {
                if (errno != ENOENT) {
                    worker_error.store(errno);
                    return;
                }
                if (stop.load()) return;
                if (std::chrono::steady_clock::now() >= deadline) {
                    worker_error.store(ETIMEDOUT);
                    stop.store(true);
                    return;
                }
                sched_yield();
            }
            if (unlink(path.c_str()) != 0) {
                worker_error.store(errno);
                return;
            }
        }
    });

    for (int i = 0; i < kFiles; ++i) {
        const std::string path = dir + "/file_" + std::to_string(i);
        int fd = open(path.c_str(), O_CREAT | O_EXCL | O_WRONLY, 0600);
        if (fd < 0) {
            worker_error.store(errno);
            stop.store(true);
            break;
        }
        close(fd);
        struct stat st {};
        while (lstat(path.c_str(), &st) == 0) {
            if (worker_error.load() != 0 || std::chrono::steady_clock::now() >= deadline) {
                if (worker_error.load() == 0) worker_error.store(ETIMEDOUT);
                stop.store(true);
                break;
            }
            sched_yield();
        }
        if (stop.load()) break;
        if (errno != ENOENT) {
            worker_error.store(errno);
            stop.store(true);
            break;
        }
    }
    remover.join();
    ASSERT_EQ(worker_error.load(), 0) << strerror(worker_error.load());

    auto evs = drain_events(ifd);
    for (int i = 0; i < kFiles; ++i) {
        const std::string name = "file_" + std::to_string(i);
        int created = -1;
        int deleted = -1;
        for (size_t j = 0; j < evs.size(); ++j) {
            if (evs[j].name != name) continue;
            if (created < 0 && (evs[j].mask & IN_CREATE)) created = static_cast<int>(j);
            if (deleted < 0 && (evs[j].mask & IN_DELETE)) deleted = static_cast<int>(j);
        }
        ASSERT_GE(created, 0) << "missing IN_CREATE for " << name;
        ASSERT_GE(deleted, 0) << "missing IN_DELETE for " << name;
        EXPECT_LT(created, deleted) << "namespace events reordered for " << name;
    }

    close(ifd);
    rmdir(dir.c_str());
}

TEST(InotifyNamespaceEvents, ConcurrentDeleteRecreatePreservesCommitOrder) {
    const std::string dir = "/tmp/dunitest_inotify_delete_recreate_order";
    ASSERT_EQ(mkdir(dir.c_str(), 0777), 0) << strerror(errno);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, dir.c_str(), IN_CREATE | IN_DELETE), 0);

    auto run_case = [&](const std::string &prefix, bool is_dir, auto create, auto remove) {
        constexpr int kEntries = 64;
        for (int i = 0; i < kEntries; ++i) {
            const std::string path = dir + "/" + prefix + std::to_string(i);
            if (create(path) != 0) return errno;
        }
        drain_events(ifd);

        std::atomic<int> worker_error{0};
        std::atomic<bool> stop{false};
        const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(5);
        std::thread remover([&]() {
            for (int i = 0; i < kEntries; ++i) {
                const std::string path = dir + "/" + prefix + std::to_string(i);
                if (remove(path) != 0) {
                    worker_error.store(errno);
                    stop.store(true);
                    return;
                }
            }
        });

        for (int i = 0; i < kEntries; ++i) {
            const std::string path = dir + "/" + prefix + std::to_string(i);
            struct stat st {};
            while (lstat(path.c_str(), &st) == 0) {
                if (worker_error.load() != 0 ||
                    std::chrono::steady_clock::now() >= deadline) {
                    if (worker_error.load() == 0) worker_error.store(ETIMEDOUT);
                    stop.store(true);
                    break;
                }
                sched_yield();
            }
            if (stop.load()) break;
            if (errno != ENOENT || create(path) != 0) {
                worker_error.store(errno);
                stop.store(true);
                break;
            }
        }
        remover.join();
        if (worker_error.load() != 0) return worker_error.load();

        auto evs = drain_events(ifd);
        for (int i = 0; i < kEntries; ++i) {
            const std::string name = prefix + std::to_string(i);
            int deleted = -1;
            int created = -1;
            uint32_t delete_mask = 0;
            uint32_t create_mask = 0;
            for (size_t j = 0; j < evs.size(); ++j) {
                if (evs[j].name != name) continue;
                if (deleted < 0 && (evs[j].mask & IN_DELETE)) {
                    deleted = static_cast<int>(j);
                    delete_mask = evs[j].mask;
                }
                if (created < 0 && (evs[j].mask & IN_CREATE)) {
                    created = static_cast<int>(j);
                    create_mask = evs[j].mask;
                }
            }
            if (deleted < 0 || created < 0 || deleted >= created) return EPROTO;
            if (!!(delete_mask & IN_ISDIR) != is_dir ||
                !!(create_mask & IN_ISDIR) != is_dir) {
                return EPROTO;
            }
        }

        for (int i = 0; i < kEntries; ++i) {
            const std::string path = dir + "/" + prefix + std::to_string(i);
            if (remove(path) != 0) return errno;
        }
        drain_events(ifd);
        return 0;
    };

    auto create_file = [](const std::string &path) {
        int fd = open(path.c_str(), O_CREAT | O_EXCL | O_WRONLY, 0600);
        if (fd < 0) return -1;
        return close(fd);
    };
    EXPECT_EQ(run_case("file_", false, create_file,
                       [](const std::string &path) { return unlink(path.c_str()); }),
              0);
    EXPECT_EQ(run_case("dir_", true,
                       [](const std::string &path) { return mkdir(path.c_str(), 0700); },
                       [](const std::string &path) { return rmdir(path.c_str()); }),
              0);
    EXPECT_EQ(run_case("link_", false,
                       [](const std::string &path) { return symlink("target", path.c_str()); },
                       [](const std::string &path) { return unlink(path.c_str()); }),
              0);

    close(ifd);
    rmdir(dir.c_str());
}

TEST(InotifyNamespaceEvents, ConcurrentMkdirPublishesOneCreate) {
    const std::string dir = "/tmp/dunitest_inotify_concurrent_mkdir";
    const std::string name = "child";
    const std::string path = dir + "/" + name;
    ASSERT_EQ(mkdir(dir.c_str(), 0777), 0) << strerror(errno);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, dir.c_str(), IN_CREATE), 0);

    std::atomic<int> ready{0};
    std::atomic<bool> go{false};
    int results[2] = {};
    auto worker = [&](int index) {
        ready.fetch_add(1);
        while (!go.load()) sched_yield();
        results[index] = mkdir(path.c_str(), 0700) == 0 ? 0 : errno;
    };
    std::thread first(worker, 0);
    std::thread second(worker, 1);
    while (ready.load() != 2) sched_yield();
    go.store(true);
    first.join();
    second.join();

    EXPECT_TRUE((results[0] == 0 && results[1] == EEXIST) ||
                (results[1] == 0 && results[0] == EEXIST));
    auto evs = drain_events(ifd);
    int creates = 0;
    for (const auto &event : evs) {
        if ((event.mask & IN_CREATE) && event.name == name) {
            ++creates;
            EXPECT_NE(event.mask & IN_ISDIR, 0U);
        }
    }
    EXPECT_EQ(creates, 1);

    close(ifd);
    rmdir(path.c_str());
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

TEST(InotifyAttribEvent, XattrSuccessNotifiesSelfAndParent) {
    const std::string dir = "/root";
    const std::string name = "dunitest_inotify_xattr";
    const std::string path = dir + "/" + name;
    const char attr[] = "user.dunitest_inotify";
    const char value[] = "value";
    unlink(path.c_str());

    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0600);
    ASSERT_GE(fd, 0) << strerror(errno);
    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, path.c_str(), IN_ATTRIB), 0);
    ASSERT_GE(inotify_add_watch(ifd, dir.c_str(), IN_ATTRIB), 0);

    auto expect_self_and_parent = [&]() {
        auto evs = drain_events(ifd);
        EXPECT_TRUE(saw_self(evs, IN_ATTRIB));
        EXPECT_TRUE(saw(evs, IN_ATTRIB, name));
    };

    int set_result = setxattr(path.c_str(), attr, value, sizeof(value), 0);
    if (set_result == -1 && (errno == ENOSYS || errno == EOPNOTSUPP)) {
        close(ifd);
        close(fd);
        unlink(path.c_str());
        GTEST_SKIP() << "root filesystem does not support extended attributes";
    }
    ASSERT_EQ(set_result, 0) << strerror(errno);
    expect_self_and_parent();

    // Linux reports a successful set even when the value is unchanged.
    ASSERT_EQ(setxattr(path.c_str(), attr, value, sizeof(value), 0), 0) << strerror(errno);
    expect_self_and_parent();

    errno = 0;
    EXPECT_EQ(setxattr(path.c_str(), attr, value, sizeof(value), XATTR_CREATE), -1);
    EXPECT_EQ(errno, EEXIST);
    auto failed_set_events = drain_events(ifd);
    EXPECT_FALSE(saw_self(failed_set_events, IN_ATTRIB));
    EXPECT_FALSE(saw(failed_set_events, IN_ATTRIB, name));

    ASSERT_EQ(fremovexattr(fd, attr), 0) << strerror(errno);
    expect_self_and_parent();

    errno = 0;
    EXPECT_EQ(removexattr(path.c_str(), attr), -1);
    EXPECT_EQ(errno, ENODATA);
    auto failed_remove_events = drain_events(ifd);
    EXPECT_FALSE(saw_self(failed_remove_events, IN_ATTRIB));
    EXPECT_FALSE(saw(failed_remove_events, IN_ATTRIB, name));

    ASSERT_EQ(fsetxattr(fd, attr, value, sizeof(value), 0), 0) << strerror(errno);
    expect_self_and_parent();

    ASSERT_EQ(lsetxattr(path.c_str(), attr, value, sizeof(value), 0), 0) << strerror(errno);
    expect_self_and_parent();
    ASSERT_EQ(lremovexattr(path.c_str(), attr), 0) << strerror(errno);
    expect_self_and_parent();

    close(ifd);
    close(fd);
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

TEST(InotifySelfEvents, RelinkFromAnotherDisconnectedAliasCancelsDelete) {
    const std::string first = "/tmp/dunitest_inotify_relink_a";
    const std::string second = "/tmp/dunitest_inotify_relink_b";
    const std::string restored = "/tmp/dunitest_inotify_relink_c";
    int first_fd = open(first.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(first_fd, 0);
    ASSERT_EQ(link(first.c_str(), second.c_str()), 0) << strerror(errno);
    int second_fd = open(second.c_str(), O_RDWR);
    ASSERT_GE(second_fd, 0);

    ASSERT_EQ(unlink(first.c_str()), 0) << strerror(errno);
    ASSERT_EQ(unlink(second.c_str()), 0) << strerror(errno);
    constexpr int kAtEmptyPath = 0x1000;
    ASSERT_EQ(linkat(first_fd, "", AT_FDCWD, restored.c_str(), kAtEmptyPath), 0)
        << strerror(errno);

    char procfd[64];
    snprintf(procfd, sizeof(procfd), "/proc/self/fd/%d", second_fd);
    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, procfd, IN_DELETE_SELF), 0) << strerror(errno);

    ASSERT_EQ(close(second_fd), 0);
    auto after_old_alias_close = drain_events(ifd);
    EXPECT_FALSE(saw_self(after_old_alias_close, IN_DELETE_SELF));
    EXPECT_FALSE(saw_self(after_old_alias_close, IN_IGNORED));

    ASSERT_EQ(close(first_fd), 0);
    ASSERT_EQ(unlink(restored.c_str()), 0) << strerror(errno);
    auto after_final_unlink = drain_events(ifd);
    EXPECT_TRUE(saw_self(after_final_unlink, IN_DELETE_SELF));
    EXPECT_TRUE(saw_self(after_final_unlink, IN_IGNORED));
    close(ifd);
}

TEST(InotifySelfEvents, ProcFdReopenIsRejectedWithoutClosingOriginalInstance) {
    const std::string path = "/tmp/dunitest_inotify_procfd_reopen";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, path.c_str(), IN_ATTRIB), 0);

    char procfd[64];
    snprintf(procfd, sizeof(procfd), "/proc/self/fd/%d", ifd);
    errno = 0;
    EXPECT_EQ(open(procfd, O_RDONLY), -1);
    EXPECT_EQ(errno, ENXIO);

    // dup() shares the original open file description and remains supported.
    int duplicate = dup(ifd);
    ASSERT_GE(duplicate, 0);
    ASSERT_EQ(close(duplicate), 0);

    ASSERT_EQ(chmod(path.c_str(), 0600), 0);
    EXPECT_TRUE(saw_self(drain_events(ifd), IN_ATTRIB));

    close(ifd);
    close(fd);
    unlink(path.c_str());
}

TEST(InotifyNamespaceEvents, HardLinkReportsAttribBeforeCreate) {
    const std::string source = "/tmp/dunitest_inotify_link_order_source";
    const std::string target = "/tmp/dunitest_inotify_link_order_target";
    int fd = open(source.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    int source_wd = inotify_add_watch(ifd, source.c_str(), IN_ATTRIB);
    ASSERT_GE(source_wd, 0);
    int parent_wd = inotify_add_watch(ifd, "/tmp", IN_CREATE);
    ASSERT_GE(parent_wd, 0);
    (void)drain_events(ifd);

    ASSERT_EQ(link(source.c_str(), target.c_str()), 0);
    auto evs = drain_events(ifd);
    int attrib = first_event_index(evs, source_wd, IN_ATTRIB);
    int create = first_event_index(evs, parent_wd, IN_CREATE);
    ASSERT_GE(attrib, 0);
    ASSERT_GE(create, 0);
    EXPECT_LT(attrib, create);
    EXPECT_TRUE(saw(evs, IN_CREATE, "dunitest_inotify_link_order_target"));

    close(ifd);
    unlink(source.c_str());
    unlink(target.c_str());
}

TEST(InotifyNamespaceEvents, ConcurrentHardLinkDeletePreservesCommitOrder) {
    const std::string dir = "/tmp/dunitest_inotify_link_commit_order";
    const std::string source = dir + "/source";
    ASSERT_EQ(mkdir(dir.c_str(), 0777), 0) << strerror(errno);
    int fd = open(source.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    int source_wd = inotify_add_watch(ifd, source.c_str(), IN_ATTRIB);
    ASSERT_GE(source_wd, 0);
    int parent_wd = inotify_add_watch(ifd, dir.c_str(), IN_CREATE | IN_DELETE);
    ASSERT_GE(parent_wd, 0);
    (void)drain_events(ifd);

    constexpr int kLinks = 64;
    std::atomic<int> worker_error{0};
    std::atomic<bool> stop{false};
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(5);
    std::thread remover([&]() {
        for (int i = 0; i < kLinks; ++i) {
            const std::string path = dir + "/link_" + std::to_string(i);
            struct stat st {};
            while (lstat(path.c_str(), &st) != 0) {
                if (errno != ENOENT) {
                    worker_error.store(errno);
                    return;
                }
                if (stop.load()) return;
                if (std::chrono::steady_clock::now() >= deadline) {
                    worker_error.store(ETIMEDOUT);
                    stop.store(true);
                    return;
                }
                sched_yield();
            }
            if (unlink(path.c_str()) != 0) {
                worker_error.store(errno);
                return;
            }
        }
    });

    for (int i = 0; i < kLinks; ++i) {
        const std::string path = dir + "/link_" + std::to_string(i);
        if (link(source.c_str(), path.c_str()) != 0) {
            worker_error.store(errno);
            stop.store(true);
            break;
        }
        struct stat st {};
        while (lstat(path.c_str(), &st) == 0) {
            if (worker_error.load() != 0 || std::chrono::steady_clock::now() >= deadline) {
                if (worker_error.load() == 0) worker_error.store(ETIMEDOUT);
                stop.store(true);
                break;
            }
            sched_yield();
        }
        if (stop.load()) break;
        if (errno != ENOENT) {
            worker_error.store(errno);
            stop.store(true);
            break;
        }
    }
    remover.join();
    ASSERT_EQ(worker_error.load(), 0) << strerror(worker_error.load());

    auto evs = drain_events(ifd);
    for (int i = 0; i < kLinks; ++i) {
        const std::string name = "link_" + std::to_string(i);
        int created = -1;
        int deleted = -1;
        for (size_t j = 0; j < evs.size(); ++j) {
            if (evs[j].wd != parent_wd || evs[j].name != name) continue;
            if (created < 0 && (evs[j].mask & IN_CREATE)) created = static_cast<int>(j);
            if (deleted < 0 && (evs[j].mask & IN_DELETE)) deleted = static_cast<int>(j);
        }
        ASSERT_GT(created, 0) << "missing IN_CREATE for " << name;
        ASSERT_GE(deleted, 0) << "missing IN_DELETE for " << name;
        EXPECT_EQ(evs[created - 1].wd, source_wd);
        EXPECT_NE(evs[created - 1].mask & IN_ATTRIB, 0U);
        EXPECT_LT(created, deleted) << "namespace events reordered for " << name;
    }

    close(ifd);
    unlink(source.c_str());
    rmdir(dir.c_str());
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

TEST(InotifySelfEvents, RenameOverEmptyDirectoryDeletesTargetWatch) {
    const std::string root = "/tmp/dunitest_inotify_rename_dir_target";
    const std::string source = root + "/source";
    const std::string target = root + "/target";
    mkdir(root.c_str(), 0777);
    mkdir(source.c_str(), 0777);
    mkdir(target.c_str(), 0777);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, target.c_str(), IN_DELETE_SELF), 0);

    ASSERT_EQ(rename(source.c_str(), target.c_str()), 0) << strerror(errno);
    auto evs = drain_events(ifd);
    EXPECT_TRUE(saw_self(evs, IN_DELETE_SELF));
    EXPECT_TRUE(saw_self(evs, IN_IGNORED));

    close(ifd);
    rmdir(target.c_str());
    rmdir(root.c_str());
}

TEST(InotifyRenameEvents, OverwriteNotifiesTargetBeforeSourceMoveSelf) {
    const std::string source = "/tmp/dunitest_inotify_rename_order_source";
    const std::string target = "/tmp/dunitest_inotify_rename_order_target";
    int fd = open(source.c_str(), O_CREAT | O_WRONLY | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);
    fd = open(target.c_str(), O_CREAT | O_WRONLY | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    int source_wd = inotify_add_watch(ifd, source.c_str(), IN_MOVE_SELF);
    int target_wd = inotify_add_watch(ifd, target.c_str(), IN_ATTRIB);
    ASSERT_GE(source_wd, 0);
    ASSERT_GE(target_wd, 0);

    ASSERT_EQ(rename(source.c_str(), target.c_str()), 0) << strerror(errno);
    auto evs = drain_events(ifd);
    int target_attrib = first_event_index(evs, target_wd, IN_ATTRIB);
    int source_move = first_event_index(evs, source_wd, IN_MOVE_SELF);
    ASSERT_GE(target_attrib, 0);
    ASSERT_GE(source_move, 0);
    EXPECT_LT(target_attrib, source_move);

    close(ifd);
    unlink(target.c_str());
}

TEST(InotifyAnonymousObjects, PipeProcFdWatchEnablesEvents) {
    int pipefd[2];
    ASSERT_EQ(pipe(pipefd), 0);
    char procfd[64];
    snprintf(procfd, sizeof(procfd), "/proc/self/fd/%d", pipefd[0]);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, procfd, IN_ACCESS | IN_MODIFY), 0)
        << strerror(errno);

    ASSERT_EQ(write(pipefd[1], "x", 1), 1);
    char byte = 0;
    ASSERT_EQ(read(pipefd[0], &byte, 1), 1);
    auto evs = drain_events(ifd);
    EXPECT_TRUE(saw_self(evs, IN_MODIFY));
    EXPECT_TRUE(saw_self(evs, IN_ACCESS));

    close(ifd);
    close(pipefd[0]);
    close(pipefd[1]);
}

TEST(InotifyAnonymousObjects, SocketProcFdWatchEnablesEvents) {
    int sockets[2];
    ASSERT_EQ(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets), 0) << strerror(errno);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(add_fd_watch(ifd, sockets[0], IN_ACCESS | IN_MODIFY), 0)
        << strerror(errno);

    ASSERT_EQ(write(sockets[1], "x", 1), 1);
    char byte = 0;
    ASSERT_EQ(read(sockets[0], &byte, 1), 1);
    ASSERT_EQ(write(sockets[0], "y", 1), 1);
    ASSERT_EQ(read(sockets[1], &byte, 1), 1);

    auto evs = drain_events(ifd);
    EXPECT_TRUE(saw_self(evs, IN_ACCESS));
    EXPECT_TRUE(saw_self(evs, IN_MODIFY));

    close(ifd);
    close(sockets[0]);
    close(sockets[1]);
}

TEST(InotifyAnonymousObjects, SpliceAndTeeNotifyBothPipeEndpoints) {
    // pipe -> pipe: Linux publishes output MODIFY before input ACCESS.
    int source[2];
    int destination[2];
    ASSERT_EQ(pipe(source), 0);
    ASSERT_EQ(pipe(destination), 0);
    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0) << strerror(errno);
    int source_wd = add_fd_watch(ifd, source[0], IN_ACCESS);
    int destination_wd = add_fd_watch(ifd, destination[1], IN_MODIFY);
    ASSERT_GE(source_wd, 0) << strerror(errno);
    ASSERT_GE(destination_wd, 0) << strerror(errno);
    ASSERT_EQ(write(source[1], "s", 1), 1);
    drain_events(ifd);
    ASSERT_EQ(splice(source[0], nullptr, destination[1], nullptr, 1, 0), 1)
        << strerror(errno);
    auto splice_events = drain_events(ifd);
    int modify_index = first_event_index(splice_events, destination_wd, IN_MODIFY);
    int access_index = first_event_index(splice_events, source_wd, IN_ACCESS);
    ASSERT_GE(modify_index, 0);
    ASSERT_GE(access_index, 0);
    EXPECT_LT(modify_index, access_index);
    close(ifd);
    close(source[0]);
    close(source[1]);
    close(destination[0]);
    close(destination[1]);

    // tee duplicates data but is still an access of the source and a
    // modification of the destination for fsnotify purposes.
    ASSERT_EQ(pipe(source), 0);
    ASSERT_EQ(pipe(destination), 0);
    ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0) << strerror(errno);
    source_wd = add_fd_watch(ifd, source[0], IN_ACCESS);
    destination_wd = add_fd_watch(ifd, destination[1], IN_MODIFY);
    ASSERT_GE(source_wd, 0) << strerror(errno);
    ASSERT_GE(destination_wd, 0) << strerror(errno);
    ASSERT_EQ(write(source[1], "t", 1), 1);
    drain_events(ifd);
    ASSERT_EQ(tee(source[0], destination[1], 1, 0), 1) << strerror(errno);
    auto tee_events = drain_events(ifd);
    access_index = first_event_index(tee_events, source_wd, IN_ACCESS);
    modify_index = first_event_index(tee_events, destination_wd, IN_MODIFY);
    ASSERT_GE(access_index, 0);
    ASSERT_GE(modify_index, 0);
    EXPECT_LT(access_index, modify_index);
    close(ifd);
    close(source[0]);
    close(source[1]);
    close(destination[0]);
    close(destination[1]);
}

TEST(InotifyAnonymousObjects, SpliceNotifiesFileAndPipeSides) {
    char path[] = "/tmp/dunitest_inotify_splice_XXXXXX";
    int filefd = mkstemp(path);
    ASSERT_GE(filefd, 0) << strerror(errno);
    ASSERT_EQ(write(filefd, "xy", 2), 2);
    ASSERT_EQ(lseek(filefd, 0, SEEK_SET), 0);

    // file -> pipe: output MODIFY and input ACCESS are emitted only after the
    // pipe accepts data.
    int pipefd[2];
    ASSERT_EQ(pipe(pipefd), 0);
    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0) << strerror(errno);
    int file_wd = add_fd_watch(ifd, filefd, IN_ACCESS | IN_MODIFY);
    int pipe_wd = add_fd_watch(ifd, pipefd[1], IN_ACCESS | IN_MODIFY);
    ASSERT_GE(file_wd, 0) << strerror(errno);
    ASSERT_GE(pipe_wd, 0) << strerror(errno);
    ASSERT_EQ(splice(filefd, nullptr, pipefd[1], nullptr, 1, 0), 1)
        << strerror(errno);
    auto file_to_pipe_events = drain_events(ifd);
    int modify_index = first_event_index(file_to_pipe_events, pipe_wd, IN_MODIFY);
    int access_index = first_event_index(file_to_pipe_events, file_wd, IN_ACCESS);
    ASSERT_GE(modify_index, 0);
    ASSERT_GE(access_index, 0);
    EXPECT_LT(modify_index, access_index);

    // pipe -> file: the regular write already owns MODIFY; splice adds the
    // missing ACCESS for the pipe source without duplicating file events.
    drain_events(ifd);
    ASSERT_EQ(lseek(filefd, 0, SEEK_SET), 0);
    ASSERT_EQ(splice(pipefd[0], nullptr, filefd, nullptr, 1, 0), 1)
        << strerror(errno);
    auto pipe_to_file_events = drain_events(ifd);
    modify_index = first_event_index(pipe_to_file_events, file_wd, IN_MODIFY);
    access_index = first_event_index(pipe_to_file_events, pipe_wd, IN_ACCESS);
    ASSERT_GE(modify_index, 0);
    ASSERT_GE(access_index, 0);
    EXPECT_LT(modify_index, access_index);

    close(ifd);
    close(pipefd[0]);
    close(pipefd[1]);
    close(filefd);
    ASSERT_EQ(unlink(path), 0);
}

TEST(InotifyAnonymousObjects, NamedFifoSpliceUsesPathIdentity) {
    const std::string root = "/tmp/dunitest_inotify_fifo_splice";
    const std::string fifo_path = root + "/fifo";
    ASSERT_EQ(mkdir(root.c_str(), 0700), 0) << strerror(errno);
    ASSERT_EQ(mkfifo(fifo_path.c_str(), 0600), 0) << strerror(errno);

    int fifo_read = open(fifo_path.c_str(), O_RDONLY | O_NONBLOCK);
    ASSERT_GE(fifo_read, 0) << strerror(errno);
    int fifo_write = open(fifo_path.c_str(), O_WRONLY | O_NONBLOCK);
    ASSERT_GE(fifo_write, 0) << strerror(errno);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0) << strerror(errno);
    int fifo_wd = inotify_add_watch(ifd, fifo_path.c_str(), IN_ACCESS | IN_MODIFY);
    int parent_wd = inotify_add_watch(ifd, root.c_str(), IN_ACCESS | IN_MODIFY);
    ASSERT_GE(fifo_wd, 0) << strerror(errno);
    ASSERT_GE(parent_wd, 0) << strerror(errno);
    int proc_ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(proc_ifd, 0) << strerror(errno);
    ASSERT_GE(add_fd_watch(proc_ifd, fifo_read, IN_ACCESS | IN_MODIFY), 0)
        << strerror(errno);

    int destination[2];
    ASSERT_EQ(pipe(destination), 0);
    ASSERT_EQ(write(fifo_write, "f", 1), 1);
    drain_events(ifd);
    ASSERT_EQ(splice(fifo_read, nullptr, destination[1], nullptr, 1, 0), 1)
        << strerror(errno);
    auto access_events = drain_events(ifd);
    EXPECT_GE(first_event_index(access_events, fifo_wd, IN_ACCESS), 0);
    EXPECT_LT(first_event_index(access_events, parent_wd, IN_ACCESS), 0)
        << "Linux suppresses special-file content events on the parent watch";
    EXPECT_TRUE(saw_self(drain_events(proc_ifd), IN_ACCESS));

    char file_path[] = "/tmp/dunitest_inotify_fifo_input_XXXXXX";
    int filefd = mkstemp(file_path);
    ASSERT_GE(filefd, 0) << strerror(errno);
    ASSERT_EQ(write(filefd, "g", 1), 1);
    ASSERT_EQ(lseek(filefd, 0, SEEK_SET), 0);
    drain_events(ifd);
    ASSERT_EQ(splice(filefd, nullptr, fifo_write, nullptr, 1, 0), 1)
        << strerror(errno);
    auto modify_events = drain_events(ifd);
    EXPECT_GE(first_event_index(modify_events, fifo_wd, IN_MODIFY), 0);
    EXPECT_LT(first_event_index(modify_events, parent_wd, IN_MODIFY), 0);
    EXPECT_TRUE(saw_self(drain_events(proc_ifd), IN_MODIFY));

    close(filefd);
    unlink(file_path);
    close(destination[0]);
    close(destination[1]);
    close(proc_ifd);
    close(ifd);
    close(fifo_read);
    close(fifo_write);
    unlink(fifo_path.c_str());
    rmdir(root.c_str());
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

TEST(InotifyMetadataEvents, KillPrivReportsWriteAndTruncateModeChanges) {
    const std::string path = "/tmp/dunitest_inotify_killpriv";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0755);
    ASSERT_GE(fd, 0);
    close(fd);
    ASSERT_EQ(chown(path.c_str(), 65534, 65534), 0);
    ASSERT_EQ(chmod(path.c_str(), 04755), 0);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, path.c_str(), IN_ATTRIB | IN_MODIFY), 0);

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (setgid(65534) != 0 || setuid(65534) != 0) _exit(10);
        int child_fd = open(path.c_str(), O_WRONLY);
        if (child_fd < 0) _exit(11);
        if (write(child_fd, "x", 1) != 1) _exit(12);
        close(child_fd);
        _exit(0);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(WEXITSTATUS(status), 0);
    auto write_events = drain_events(ifd);
    int attrib_index = first_event_index(write_events, IN_ATTRIB);
    int modify_index = first_event_index(write_events, IN_MODIFY);
    ASSERT_GE(attrib_index, 0);
    ASSERT_GE(modify_index, 0);
    EXPECT_LT(attrib_index, modify_index);

    ASSERT_EQ(chmod(path.c_str(), 04755), 0);
    (void)drain_events(ifd);
    child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (setgid(65534) != 0 || setuid(65534) != 0) _exit(20);
        int child_fd = open(path.c_str(), O_WRONLY);
        if (child_fd < 0) _exit(21);
        if (ftruncate(child_fd, 2) != 0) _exit(22);
        close(child_fd);
        _exit(0);
    }
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(WEXITSTATUS(status), 0);
    auto truncate_events = drain_events(ifd);
    bool saw_combined = false;
    for (const auto &event : truncate_events) {
        if ((event.mask & (IN_ATTRIB | IN_MODIFY)) == (IN_ATTRIB | IN_MODIFY)) {
            saw_combined = true;
        }
    }
    EXPECT_TRUE(saw_combined);

    // A non-executable SGID bit is preserved when the writer belongs to the
    // file's group; no mode change means no ATTRIB event.
    ASSERT_EQ(chmod(path.c_str(), 02644), 0);
    (void)drain_events(ifd);
    child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (setgid(65534) != 0 || setuid(65534) != 0) _exit(30);
        int child_fd = open(path.c_str(), O_WRONLY);
        if (child_fd < 0) _exit(31);
        if (write(child_fd, "y", 1) != 1) _exit(32);
        close(child_fd);
        _exit(0);
    }
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(WEXITSTATUS(status), 0);
    auto preserved_sgid_events = drain_events(ifd);
    EXPECT_FALSE(saw_self(preserved_sgid_events, IN_ATTRIB));
    EXPECT_TRUE(saw_self(preserved_sgid_events, IN_MODIFY));
    struct stat st = {};
    ASSERT_EQ(stat(path.c_str(), &st), 0);
    EXPECT_NE(st.st_mode & S_ISGID, 0);

    close(ifd);
    unlink(path.c_str());
}

TEST(InotifyMetadataEvents, FallocateKillPrivPrecedesModify) {
    const std::string path = "/tmp/dunitest_inotify_fallocate_killpriv";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0755);
    ASSERT_GE(fd, 0);
    ASSERT_EQ(ftruncate(fd, 1), 0);
    close(fd);
    ASSERT_EQ(chown(path.c_str(), 65534, 65534), 0);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, path.c_str(), IN_ATTRIB | IN_MODIFY), 0);

    auto run_unprivileged_fallocate = [&](off_t offset, off_t len, int exit_base) {
        pid_t child = fork();
        ASSERT_GE(child, 0);
        if (child == 0) {
            if (setgid(65534) != 0 || setuid(65534) != 0) _exit(exit_base);
            int child_fd = open(path.c_str(), O_RDWR);
            if (child_fd < 0) _exit(exit_base + 1);
            if (syscall(SYS_fallocate, child_fd, 0, offset, len) != 0)
                _exit(exit_base + 2);
            close(child_fd);
            _exit(0);
        }
        int status = 0;
        ASSERT_EQ(waitpid(child, &status, 0), child);
        ASSERT_TRUE(WIFEXITED(status));
        ASSERT_EQ(WEXITSTATUS(status), 0);
    };

    ASSERT_EQ(chmod(path.c_str(), 04755), 0);
    (void)drain_events(ifd);
    run_unprivileged_fallocate(0, 8192, 10);
    auto extending = drain_events(ifd);
    ASSERT_GE(first_event_index(extending, IN_ATTRIB), 0);
    ASSERT_GE(first_event_index(extending, IN_MODIFY), 0);
    EXPECT_LT(first_event_index(extending, IN_ATTRIB),
              first_event_index(extending, IN_MODIFY));
    struct stat st = {};
    ASSERT_EQ(stat(path.c_str(), &st), 0);
    EXPECT_EQ(st.st_mode & S_ISUID, 0);

    ASSERT_EQ(chmod(path.c_str(), 04755), 0);
    (void)drain_events(ifd);
    run_unprivileged_fallocate(0, 4096, 20);
    auto within_eof = drain_events(ifd);
    ASSERT_GE(first_event_index(within_eof, IN_ATTRIB), 0);
    ASSERT_GE(first_event_index(within_eof, IN_MODIFY), 0);
    EXPECT_LT(first_event_index(within_eof, IN_ATTRIB),
              first_event_index(within_eof, IN_MODIFY));
    ASSERT_EQ(stat(path.c_str(), &st), 0);
    EXPECT_EQ(st.st_mode & S_ISUID, 0);
    EXPECT_EQ(st.st_size, 8192);

    (void)drain_events(ifd);
    fd = open(path.c_str(), O_RDWR);
    ASSERT_GE(fd, 0);
    errno = 0;
    EXPECT_EQ(syscall(SYS_fallocate, fd, -1, 0, 4096), -1);
    EXPECT_TRUE(drain_events(ifd).empty());
    close(fd);
    close(ifd);
    unlink(path.c_str());
}

TEST(InotifyDataEvents, BulkCopiesPublishOneEventPerEndpoint) {
    const std::string source = "/tmp/dunitest_inotify_bulk_source";
    const std::string target = "/tmp/dunitest_inotify_bulk_target";
    constexpr size_t kTransferSize = 12288;
    std::vector<char> data(kTransferSize, 'x');

    int source_fd = open(source.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    int target_fd = open(target.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(source_fd, 0);
    ASSERT_GE(target_fd, 0);
    ASSERT_EQ(write(source_fd, data.data(), data.size()),
              static_cast<ssize_t>(data.size()));
    ASSERT_EQ(lseek(source_fd, 0, SEEK_SET), 0);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    int source_wd = inotify_add_watch(ifd, source.c_str(), IN_ACCESS);
    int target_wd = inotify_add_watch(ifd, target.c_str(), IN_MODIFY);
    ASSERT_GE(source_wd, 0);
    ASSERT_GE(target_wd, 0);

    ASSERT_EQ(syscall(SYS_copy_file_range, source_fd, nullptr, target_fd, nullptr,
                      kTransferSize, 0),
              static_cast<ssize_t>(kTransferSize));
    auto copy_events = drain_events(ifd);
    ASSERT_EQ(copy_events.size(), 2u);
    EXPECT_EQ(copy_events[0].wd, source_wd);
    EXPECT_TRUE(copy_events[0].mask & IN_ACCESS);
    EXPECT_EQ(copy_events[1].wd, target_wd);
    EXPECT_TRUE(copy_events[1].mask & IN_MODIFY);

    ASSERT_EQ(lseek(source_fd, 0, SEEK_SET), 0);
    ASSERT_EQ(ftruncate(target_fd, 0), 0);
    ASSERT_EQ(lseek(target_fd, 0, SEEK_SET), 0);
    (void)drain_events(ifd);
    ASSERT_EQ(syscall(SYS_sendfile, target_fd, source_fd, nullptr, kTransferSize),
              static_cast<ssize_t>(kTransferSize));
    auto sendfile_events = drain_events(ifd);
    ASSERT_EQ(sendfile_events.size(), 2u);
    EXPECT_EQ(sendfile_events[0].wd, source_wd);
    EXPECT_TRUE(sendfile_events[0].mask & IN_ACCESS);
    EXPECT_EQ(sendfile_events[1].wd, target_wd);
    EXPECT_TRUE(sendfile_events[1].mask & IN_MODIFY);

    close(ifd);
    close(source_fd);
    close(target_fd);
    unlink(source.c_str());
    unlink(target.c_str());
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

TEST(InotifyOpenEvents, OTruncKillPrivUsesOneCombinedMetadataEvent) {
    const std::string path = "/tmp/dunitest_inotify_otrunc_killpriv";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0755);
    ASSERT_GE(fd, 0);
    ASSERT_EQ(write(fd, "payload", 7), 7);
    close(fd);
    ASSERT_EQ(chown(path.c_str(), 65534, 65534), 0);
    ASSERT_EQ(chmod(path.c_str(), 04755), 0);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, path.c_str(), IN_OPEN | IN_ATTRIB | IN_MODIFY), 0);

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (setgid(65534) != 0 || setuid(65534) != 0) _exit(10);
        int child_fd = open(path.c_str(), O_WRONLY | O_TRUNC);
        if (child_fd < 0) _exit(11);
        close(child_fd);
        _exit(0);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(WEXITSTATUS(status), 0);

    auto evs = drain_events(ifd);
    int open_index = first_event_index(evs, IN_OPEN);
    int combined_index = -1;
    int combined_count = 0;
    for (size_t i = 0; i < evs.size(); ++i) {
        if ((evs[i].mask & (IN_ATTRIB | IN_MODIFY)) == (IN_ATTRIB | IN_MODIFY)) {
            combined_index = static_cast<int>(i);
            combined_count++;
        }
    }
    ASSERT_GE(open_index, 0);
    ASSERT_GE(combined_index, 0);
    EXPECT_LT(open_index, combined_index);
    EXPECT_EQ(combined_count, 1);
    struct stat st = {};
    ASSERT_EQ(stat(path.c_str(), &st), 0);
    EXPECT_EQ(st.st_mode & S_ISUID, 0);
    EXPECT_EQ(st.st_size, 0);

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
// FIONREAD reports the exact serialized size of all currently queued events.
// ---------------------------------------------------------------------------
TEST(InotifyIoctl, FionreadReportsQueuedBytesWithoutConsumingEvents) {
    const std::string dir =
        "/tmp/dunitest_inotify_fionread_" + std::to_string(getpid());
    const std::string name = "file";
    const std::string path = dir + "/" + name;
    InotifyTestCleanup cleanup{path, dir};
    ASSERT_EQ(0, mkdir(dir.c_str(), 0700)) << strerror(errno);
    cleanup.directory_created = true;

    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    cleanup.file_created = true;
    close(fd);

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0) << strerror(errno);
    cleanup.ifd = ifd;
    int parent_wd = inotify_add_watch(ifd, dir.c_str(), IN_ATTRIB);
    int self_wd = inotify_add_watch(ifd, path.c_str(), IN_ATTRIB);
    ASSERT_GE(parent_wd, 0) << strerror(errno);
    cleanup.parent_wd = parent_wd;
    ASSERT_GE(self_wd, 0) << strerror(errno);
    cleanup.self_wd = self_wd;

    ASSERT_EQ(0, chmod(path.c_str(), 0600)) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, ioctl(ifd, FIONREAD,
                       reinterpret_cast<int *>(static_cast<uintptr_t>(-1))));
    EXPECT_EQ(EFAULT, errno);

    int available = -1;
    ASSERT_EQ(0, ioctl(ifd, FIONREAD, &available)) << strerror(errno);
    const size_t expected = inotify_record_size(name) + inotify_record_size("");
    ASSERT_EQ(static_cast<int>(expected), available);

    std::vector<char> records(static_cast<size_t>(available));
    ASSERT_EQ(static_cast<ssize_t>(records.size()),
              read(ifd, records.data(), records.size()))
        << strerror(errno);
    ASSERT_EQ(0, ioctl(ifd, FIONREAD, &available)) << strerror(errno);
    EXPECT_EQ(0, available);

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

TEST(InotifyConcurrentReaders, BlockingReaderDoesNotHideNonblockingState) {
    const std::string path = "/tmp/dunitest_inotify_readers";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    close(fd);

    int ifd = inotify_init1(IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(inotify_add_watch(ifd, path.c_str(), IN_ATTRIB), 0);

    struct ReadAttempt {
        int fd;
        std::atomic<bool> entered{false};
        std::atomic<bool> done{false};
        ssize_t result = 0;
        int error = 0;
    } first{ifd}, second{ifd};
    auto reader = [](void *opaque) -> void * {
        auto *attempt = static_cast<ReadAttempt *>(opaque);
        char buf[64] __attribute__((aligned(8)));
        attempt->entered.store(true);
        attempt->result = read(attempt->fd, buf, sizeof(buf));
        attempt->error = errno;
        attempt->done.store(true);
        return nullptr;
    };

    pthread_t first_thread;
    ASSERT_EQ(pthread_create(&first_thread, nullptr, reader, &first), 0);
    while (!first.entered.load()) usleep(1000);
    usleep(20000);

    int flags = fcntl(ifd, F_GETFL);
    ASSERT_GE(flags, 0);
    ASSERT_EQ(fcntl(ifd, F_SETFL, flags | O_NONBLOCK), 0);
    pthread_t second_thread;
    ASSERT_EQ(pthread_create(&second_thread, nullptr, reader, &second), 0);
    for (int i = 0; i < 500 && !second.done.load(); ++i) usleep(1000);
    EXPECT_TRUE(second.done.load())
        << "a sleeping reader kept the shared consumer lock";

    ASSERT_EQ(chmod(path.c_str(), 0600), 0);
    ASSERT_EQ(pthread_join(first_thread, nullptr), 0);
    ASSERT_EQ(pthread_join(second_thread, nullptr), 0);
    EXPECT_EQ(second.result, -1);
    EXPECT_TRUE(second.error == EAGAIN || second.error == EWOULDBLOCK);

    close(ifd);
    unlink(path.c_str());
}

TEST(InotifyReadBoundary, NonblockingExact64kReturnsConsumedPrefix) {
    const std::string path = "/tmp/dunitest_inotify_read_64k_nonblock";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    queue_exact_64k_self_events(ifd, fd);

    std::vector<char> buf(128 * 1024);
    errno = 0;
    ssize_t n = read(ifd, buf.data(), buf.size());
    EXPECT_EQ(n, 64 * 1024)
        << "a second empty read must not replace consumed progress: " << strerror(errno);

    close(ifd);
    close(fd);
    unlink(path.c_str());
}

TEST(InotifyReadBoundary, PartialUserFaultConsumesFailingRecordAndReturnsEfault) {
    const std::string path = "/tmp/dunitest_inotify_read_partial_fault";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    ASSERT_GE(add_fd_watch(ifd, fd, IN_ATTRIB | IN_MODIFY), 0);
    ASSERT_EQ(fchmod(fd, 0600), 0);
    const char byte = 'x';
    ASSERT_EQ(pwrite(fd, &byte, 1, 0), 1);

    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    char *mapping = static_cast<char *>(
        mmap(nullptr, page_size * 2, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(mapping, MAP_FAILED);
    ASSERT_EQ(munmap(mapping + page_size, page_size), 0);

    errno = 0;
    EXPECT_EQ(read(ifd, mapping + page_size - 16, 32), -1);
    EXPECT_EQ(errno, EFAULT);

    char event[16] __attribute__((aligned(8)));
    errno = 0;
    EXPECT_EQ(read(ifd, event, sizeof(event)), -1);
    EXPECT_TRUE(errno == EAGAIN || errno == EWOULDBLOCK)
        << "the record whose copy faulted must still be consumed";

    ASSERT_EQ(munmap(mapping, page_size), 0);
    close(ifd);
    close(fd);
    unlink(path.c_str());
}

TEST(InotifyReadBoundary, BlockingExact64kDoesNotWaitForAnotherEvent) {
    const std::string path = "/tmp/dunitest_inotify_read_64k_blocking";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
    ASSERT_GE(fd, 0);
    int ifd = inotify_init1(IN_CLOEXEC);
    ASSERT_GE(ifd, 0);
    queue_exact_64k_self_events(ifd, fd);

    std::thread delayed_producer([fd] {
        usleep(500000);
        EXPECT_EQ(fchmod(fd, 0600), 0) << strerror(errno);
    });
    std::vector<char> buf(128 * 1024);
    ssize_t n = read(ifd, buf.data(), buf.size());
    EXPECT_EQ(n, 64 * 1024)
        << "a successful record-stream read must not enter a second wait cycle";
    delayed_producer.join();

    close(ifd);
    close(fd);
    unlink(path.c_str());
}

int main(int argc, char **argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
