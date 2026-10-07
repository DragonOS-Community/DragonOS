#include <gtest/gtest.h>

#include "cap_common.h"
#include "loop_ext4_test_support.h"

#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/inotify.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

#include <atomic>
#include <string>
#include <thread>
#include <vector>

#ifndef SYS_open_tree
#define SYS_open_tree 428
#endif
#ifndef SYS_move_mount
#define SYS_move_mount 429
#endif
#ifndef SYS_mount_setattr
#define SYS_mount_setattr 442
#endif
#ifndef SYS_renameat2
#define SYS_renameat2 316
#endif

namespace {

constexpr unsigned kEmptyPath = 0x1000, kClone = 1, kMoveEmpty = 4;
constexpr uint64_t kIdmap = 0x100000, kReadonly = 1;
constexpr unsigned kExchange = 2;

struct MountAttr {
    uint64_t set = 0, clear = 0, propagation = 0, userns_fd = 0;
};

class Fd {
public:
    explicit Fd(int fd = -1) : fd_(fd) {}
    ~Fd() { if (fd_ >= 0) close(fd_); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return fd_; }
    int release() { const int fd = fd_; fd_ = -1; return fd; }
    int close_now() { return close(release()); }
    void reset(int fd) { if (fd_ >= 0) close(fd_); fd_ = fd; }
private:
    int fd_;
};

int attributes(int fd, const MountAttr& attr) {
    return syscall(SYS_mount_setattr, fd, "", kEmptyPath, &attr, sizeof(attr));
}

std::vector<uint32_t> event_masks(int fd) {
    std::vector<uint32_t> masks;
    char bytes[4096];
    const ssize_t count = read(fd, bytes, sizeof(bytes));
    if (count < 0) {
        if (errno != EAGAIN) ADD_FAILURE() << "inotify read: " << strerror(errno);
        return masks;
    }
    size_t offset = 0;
    while (offset + sizeof(inotify_event) <= static_cast<size_t>(count)) {
        inotify_event event {};
        memcpy(&event, bytes + offset, sizeof(event));
        if (event.len > static_cast<size_t>(count) - offset - sizeof(event)) {
            ADD_FAILURE() << "malformed inotify event";
            return masks;
        }
        masks.push_back(event.mask);
        offset += sizeof(event) + event.len;
    }
    EXPECT_EQ(static_cast<size_t>(count), offset);
    return masks;
}

class NamespaceChild {
public:
    NamespaceChild(pid_t child, int release) : child_(child), release_(release) {}
    ~NamespaceChild() {
        const char ack = 0;
        EXPECT_EQ(1, write(release_.get(), &ack, 1));
        release_.close_now();
        int status = 0;
        EXPECT_EQ(child_, waitpid(child_, &status, 0));
        EXPECT_TRUE(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    }
private:
    pid_t child_;
    Fd release_;
};

void write_text(const std::string& path, const char* text) {
    Fd fd(open(path.c_str(), O_WRONLY));
    ASSERT_GE(fd.get(), 0) << path << ": " << strerror(errno);
    ASSERT_EQ(static_cast<ssize_t>(strlen(text)), write(fd.get(), text, strlen(text)))
        << path << ": " << strerror(errno);
}

void mapped_namespace(Fd* output) {
    int ready[2]; ASSERT_EQ(0, pipe(ready));
    Fd ready_read(ready[0]), ready_write(ready[1]);
    int release[2]; ASSERT_EQ(0, pipe(release));
    Fd release_read(release[0]), release_write(release[1]);
    const pid_t child = fork(); ASSERT_GE(child, 0);
    if (child == 0) {
        ready_read.close_now(); release_write.close_now();
        const int error = unshare(CLONE_NEWUSER) == 0 ? 0 : errno;
        if (write(ready_write.get(), &error, sizeof(error)) != sizeof(error)) _exit(2);
        char ack;
        if (read(release_read.get(), &ack, 1) != 1) _exit(3);
        _exit(0);
    }
    ready_write.close_now(); release_read.close_now();
    NamespaceChild held(child, release_write.release());
    int error = EIO;
    ASSERT_EQ(static_cast<ssize_t>(sizeof(error)), read(ready_read.get(), &error, sizeof(error)));
    ASSERT_EQ(0, error) << "CLONE_NEWUSER is a required interface: " << strerror(error);
    const std::string task = "/proc/" + std::to_string(child);
    ASSERT_NO_FATAL_FAILURE(write_text(task + "/uid_map", "0 1000 100\n200 1300 20\n"));
    ASSERT_NO_FATAL_FAILURE(write_text(task + "/setgroups", "deny\n"));
    ASSERT_NO_FATAL_FAILURE(write_text(task + "/gid_map", "0 2000 100\n200 2300 20\n"));
    output->reset(open((task + "/ns/user").c_str(), O_RDONLY | O_CLOEXEC));
    ASSERT_GE(output->get(), 0) << strerror(errno);
}

struct WorkerResult { int line = 0; int error = 0; };
#define WORKER_CHECK(condition) \
    do { if (!(condition)) return WorkerResult{__LINE__, errno}; } while (0)

template <typename Action, typename ParentAction>
void as_user_with_parent(uid_t uid, gid_t gid, Action action, ParentAction parent_action) {
    int report[2]; ASSERT_EQ(0, pipe(report));
    Fd read_end(report[0]), write_end(report[1]);
    const pid_t child = fork(); ASSERT_GE(child, 0);
    if (child == 0) {
        read_end.close_now();
        WorkerResult result;
        if (setgroups(0, nullptr) != 0 || setgid(gid) != 0 || setuid(uid) != 0) {
            result = {__LINE__, errno};
        } else {
            cap_user_data_t zero[2] = {};
            const int error = capset_errno(_LINUX_CAPABILITY_VERSION_3, 0, zero);
            if (error != 0) result = {__LINE__, error};
            else { umask(0); result = action(); }
        }
        if (write(write_end.get(), &result, sizeof(result)) != sizeof(result)) _exit(2);
        _exit(0);
    }
    write_end.close_now();
    const WorkerResult parent_result = parent_action();
    WorkerResult result;
    const ssize_t count = read(read_end.get(), &result, sizeof(result));
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status) && WEXITSTATUS(status) == 0) << status;
    ASSERT_EQ(static_cast<ssize_t>(sizeof(result)), count);
    ASSERT_EQ(0, parent_result.line) << "parent line=" << parent_result.line
        << " errno=" << parent_result.error << " " << strerror(parent_result.error);
    ASSERT_EQ(0, result.line) << "worker uid=" << uid << " gid=" << gid
        << " line=" << result.line << " errno=" << result.error << " " << strerror(result.error);
}

template <typename Action>
void as_user(uid_t uid, gid_t gid, Action action) {
    as_user_with_parent(uid, gid, action, [] { return WorkerResult{}; });
}

class MountIdmapTest : public ::testing::TestWithParam<bool> {
protected:
    void SetUp() override {
        ASSERT_EQ(0, unshare(CLONE_NEWNS)) << strerror(errno);
        ASSERT_EQ(0, mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr)) << strerror(errno);
        struct stat st {};
        if (stat("/tmp", &st) != 0) { ASSERT_EQ(0, mkdir("/tmp", 0777)); }
        char name[] = "/tmp/mount_idmap_XXXXXX";
        char* directory = mkdtemp(name); ASSERT_NE(nullptr, directory);
        root_ = directory;
        ASSERT_EQ(0, chmod(root_.c_str(), 0755));
        if (GetParam()) {
            ASSERT_NO_FATAL_FAILURE(loop_.SetUp());
            ASSERT_NO_FATAL_FAILURE(loop_.Mount());
            raw_ = loop_.mount_point();
        } else {
            raw_ = root_ + "/raw";
            ASSERT_EQ(0, mkdir(raw_.c_str(), 0755));
            ASSERT_EQ(0, mount("idmap", raw_.c_str(), "tmpfs", 0, "mode=0777"))
                << strerror(errno);
        }
        raw_mounted_ = true;
        ASSERT_EQ(0, chmod(raw_.c_str(), 0777));
        ASSERT_NO_FATAL_FAILURE(mapped_namespace(&namespace_));
        view_ = root_ + "/view";
        ASSERT_EQ(0, mkdir(view_.c_str(), 0755));
        ASSERT_NO_FATAL_FAILURE(mapped_clone(raw_, view_));
    }

    void TearDown() override {
        for (auto it = aliases_.rbegin(); it != aliases_.rend(); ++it) {
            EXPECT_EQ(0, umount2(it->c_str(), MNT_DETACH)) << *it << ": " << strerror(errno);
            EXPECT_EQ(0, rmdir(it->c_str()));
        }
        if (raw_mounted_) {
            if (GetParam()) loop_.Unmount();
            else { EXPECT_EQ(0, umount(raw_.c_str())); EXPECT_EQ(0, rmdir(raw_.c_str())); }
        }
        namespace_.reset(-1);
        if (!root_.empty()) {
            // Also remove a prepared directory if a prerequisite assertion
            // failed before its mount was published/accounted in aliases_.
            for (const auto& path : {view_, root_ + "/clone"}) {
                if (!path.empty() && rmdir(path.c_str()) != 0 && errno != ENOENT)
                    ADD_FAILURE() << path << ": " << strerror(errno);
            }
            EXPECT_EQ(0, rmdir(root_.c_str()));
        }
    }

    void mapped_clone(const std::string& source, const std::string& target) {
        Fd tree(syscall(SYS_open_tree, AT_FDCWD, source.c_str(), kClone | O_CLOEXEC));
        ASSERT_GE(tree.get(), 0) << strerror(errno);
        MountAttr attr; attr.set = kIdmap; attr.userns_fd = namespace_.get();
        ASSERT_EQ(0, attributes(tree.get(), attr)) << "real idmap capability required: " << strerror(errno);
        ASSERT_EQ(0, syscall(SYS_move_mount, tree.get(), "", AT_FDCWD, target.c_str(), kMoveEmpty))
            << strerror(errno);
        aliases_.push_back(target);
    }

    void raw_file(const char* name, uid_t uid, gid_t gid, mode_t mode = 0644) {
        const std::string path = raw_ + "/" + name;
        {
            Fd fd(open(path.c_str(), O_CREAT | O_EXCL | O_WRONLY, 0600));
            ASSERT_GE(fd.get(), 0) << strerror(errno);
            ASSERT_EQ(4, write(fd.get(), "data", 4));
        }
        ASSERT_EQ(0, chown(path.c_str(), uid, gid));
        ASSERT_EQ(0, chmod(path.c_str(), mode));
    }

    void raw_directory(const char* name, uid_t uid, gid_t gid, mode_t mode) {
        const std::string path = raw_ + "/" + name;
        ASSERT_EQ(0, mkdir(path.c_str(), 0700));
        ASSERT_EQ(0, chown(path.c_str(), uid, gid));
        ASSERT_EQ(0, chmod(path.c_str(), mode));
    }

    void owner(const std::string& path, uid_t uid, gid_t gid) {
        struct stat st {};
        ASSERT_EQ(0, lstat(path.c_str(), &st)) << path << ": " << strerror(errno);
        EXPECT_EQ(uid, st.st_uid) << path;
        EXPECT_EQ(gid, st.st_gid) << path;
    }

    std::string root_, raw_, view_;
    std::vector<std::string> aliases_;
    dunitest::LoopExt4 loop_;
    Fd namespace_;
    bool raw_mounted_ = false;
};

TEST_P(MountIdmapTest, MetadataProjectsDistinctIdsWithoutChangingBacking) {
    owner(raw_, 0, 0); owner(view_, 1000, 2000);
    ASSERT_NO_FATAL_FAILURE(raw_file("file", 17, 23));
    owner(raw_ + "/file", 17, 23); owner(view_ + "/file", 1017, 2023);
    struct stat raw {}, view {};
    ASSERT_EQ(0, stat((raw_ + "/file").c_str(), &raw));
    ASSERT_EQ(0, stat((view_ + "/file").c_str(), &view));
    EXPECT_EQ(raw.st_ino, view.st_ino); EXPECT_EQ(raw.st_dev, view.st_dev);
    ASSERT_EQ(0, chown((raw_ + "/file").c_str(), 201, 202));
    owner(view_ + "/file", 1301, 2302);
    owner(raw_ + "/file", 201, 202);
}

TEST_P(MountIdmapTest, AllCreationPathsStoreRawCallerIds) {
    ASSERT_NO_FATAL_FAILURE(as_user(1003, 2004, [&] {
        Fd file(open((view_ + "/file").c_str(), O_CREAT | O_EXCL | O_RDWR, 0644));
        WORKER_CHECK(file.get() >= 0);
        WORKER_CHECK(mkdir((view_ + "/dir").c_str(), 0755) == 0);
        WORKER_CHECK(symlink("file", (view_ + "/symlink").c_str()) == 0);
        WORKER_CHECK(mknod((view_ + "/fifo").c_str(), S_IFIFO | 0644, 0) == 0);
        WORKER_CHECK(mknod((view_ + "/regular").c_str(), S_IFREG | 0644, 0) == 0);
        Fd temp(open(view_.c_str(), O_TMPFILE | O_RDWR, 0600));
        WORKER_CHECK(temp.get() >= 0);
        struct stat st {};
        WORKER_CHECK(fstat(temp.get(), &st) == 0 && st.st_uid == 1003 && st.st_gid == 2004);
        const std::string procfd = "/proc/self/fd/" + std::to_string(temp.get());
        WORKER_CHECK(linkat(AT_FDCWD, procfd.c_str(), AT_FDCWD,
                            (view_ + "/tmp-linked").c_str(), AT_SYMLINK_FOLLOW) == 0);
        return WorkerResult{};
    }));
    for (const char* name : {"file", "dir", "symlink", "fifo", "regular", "tmp-linked"}) {
        owner(raw_ + "/" + name, 3, 4); owner(view_ + "/" + name, 1003, 2004);
    }
}

TEST_P(MountIdmapTest, UnmappedCallerCannotCreateAndSgidDoesNotBypassGidCheck) {
    Fd denied(open((view_ + "/denied").c_str(), O_CREAT | O_WRONLY, 0644));
    EXPECT_EQ(-1, denied.get()); EXPECT_EQ(EOVERFLOW, errno);
    struct stat st {};
    EXPECT_EQ(-1, stat((raw_ + "/denied").c_str(), &st)); EXPECT_EQ(ENOENT, errno);
    ASSERT_NO_FATAL_FAILURE(raw_directory("sgid", 3, 7, 02777));
    ASSERT_NO_FATAL_FAILURE(as_user(1003, 2999, [&] {
        Fd file(open((view_ + "/sgid/denied").c_str(), O_CREAT | O_WRONLY, 0644));
        WORKER_CHECK(file.get() == -1 && errno == EOVERFLOW);
        WORKER_CHECK(mkdir((view_ + "/sgid/denied-dir").c_str(), 0755) == -1 && errno == EOVERFLOW);
        return WorkerResult{};
    }));
    EXPECT_EQ(-1, stat((raw_ + "/sgid/denied").c_str(), &st)); EXPECT_EQ(ENOENT, errno);
}

TEST_P(MountIdmapTest, ChmodChownInverseAndUnmappedOwnerRepairAreAtomic) {
    ASSERT_NO_FATAL_FAILURE(raw_file("file", 3, 4));
    ASSERT_NO_FATAL_FAILURE(as_user(1003, 2004, [&] {
        WORKER_CHECK(chmod((view_ + "/file").c_str(), 0600) == 0);
        return WorkerResult{};
    }));
    owner(raw_ + "/file", 3, 4);
    ASSERT_EQ(0, chown((view_ + "/file").c_str(), 1011, 2022));
    owner(raw_ + "/file", 11, 22);
    EXPECT_EQ(-1, chown((view_ + "/file").c_str(), 9999, static_cast<gid_t>(-1)));
    EXPECT_EQ(EOVERFLOW, errno); owner(raw_ + "/file", 11, 22);
    EXPECT_EQ(-1, chown((view_ + "/file").c_str(), static_cast<uid_t>(-1), 9999));
    EXPECT_EQ(EOVERFLOW, errno); owner(raw_ + "/file", 11, 22);
    ASSERT_NO_FATAL_FAILURE(raw_file("hole", 150, 160));
    owner(view_ + "/hole", 65534, 65534);
    EXPECT_EQ(-1, chmod((view_ + "/hole").c_str(), 0600)); EXPECT_EQ(EOVERFLOW, errno);
    EXPECT_EQ(-1, chown((view_ + "/hole").c_str(), 1003, static_cast<gid_t>(-1)));
    EXPECT_EQ(EOVERFLOW, errno); owner(raw_ + "/hole", 150, 160);
    ASSERT_EQ(0, chown((view_ + "/hole").c_str(), 1003, 2004)) << strerror(errno);
    owner(raw_ + "/hole", 3, 4); owner(view_ + "/hole", 1003, 2004);
}

TEST_P(MountIdmapTest, SgidInheritanceAndStrippingUseMappedParentGroup) {
    ASSERT_NO_FATAL_FAILURE(raw_directory("sgid", 3, 7, 02777));
    ASSERT_NO_FATAL_FAILURE(as_user(1003, 2004, [&] {
        WORKER_CHECK(mkdir((view_ + "/sgid/dir").c_str(), 0755) == 0);
        Fd executable(open((view_ + "/sgid/executable").c_str(), O_CREAT | O_WRONLY, 02755));
        WORKER_CHECK(executable.get() >= 0);
        Fd nonexec(open((view_ + "/sgid/nonexec").c_str(), O_CREAT | O_WRONLY, 02644));
        WORKER_CHECK(nonexec.get() >= 0);
        return WorkerResult{};
    }));
    ASSERT_NO_FATAL_FAILURE(as_user(1003, 2007, [&] {
        Fd file(open((view_ + "/sgid/member").c_str(), O_CREAT | O_WRONLY, 02755));
        WORKER_CHECK(file.get() >= 0);
        return WorkerResult{};
    }));
    for (const char* name : {"dir", "executable", "nonexec", "member"})
        owner(raw_ + "/sgid/" + name, 3, 7);
    struct stat st {};
    ASSERT_EQ(0, stat((raw_ + "/sgid/dir").c_str(), &st)); EXPECT_NE(0u, st.st_mode & S_ISGID);
    ASSERT_EQ(0, stat((raw_ + "/sgid/executable").c_str(), &st)); EXPECT_EQ(0u, st.st_mode & S_ISGID);
    ASSERT_EQ(0, stat((raw_ + "/sgid/nonexec").c_str(), &st)); EXPECT_NE(0u, st.st_mode & S_ISGID);
    ASSERT_EQ(0, stat((raw_ + "/sgid/member").c_str(), &st)); EXPECT_NE(0u, st.st_mode & S_ISGID);
}

TEST_P(MountIdmapTest, StickyDeleteAndUnmappedVictimUseMappedIdentity) {
    ASSERT_NO_FATAL_FAILURE(raw_directory("sticky", 1, 1, 01777));
    ASSERT_NO_FATAL_FAILURE(raw_file("sticky/victim", 3, 4, 0666));
    ASSERT_NO_FATAL_FAILURE(as_user(1005, 2004, [&] {
        WORKER_CHECK(unlink((view_ + "/sticky/victim").c_str()) == -1 && errno == EPERM);
        return WorkerResult{};
    }));
    ASSERT_NO_FATAL_FAILURE(as_user(1003, 2004, [&] {
        WORKER_CHECK(unlink((view_ + "/sticky/victim").c_str()) == 0);
        return WorkerResult{};
    }));
    ASSERT_NO_FATAL_FAILURE(raw_file("hole", 150, 4, 0666));
    EXPECT_EQ(-1, unlink((view_ + "/hole").c_str())); EXPECT_EQ(EOVERFLOW, errno);
    owner(raw_ + "/hole", 150, 4);
    ASSERT_NO_FATAL_FAILURE(raw_directory("hole-dir", 3, 160, 0777));
    EXPECT_EQ(-1, rmdir((view_ + "/hole-dir").c_str())); EXPECT_EQ(EOVERFLOW, errno);
    owner(raw_ + "/hole-dir", 3, 160);
}

TEST_P(MountIdmapTest, HardlinksPreserveRawOwnerAndRejectDifferentMountOrHole) {
    ASSERT_NO_FATAL_FAILURE(raw_file("file", 3, 4));
    ASSERT_NO_FATAL_FAILURE(as_user(1003, 2004, [&] {
        WORKER_CHECK(link((view_ + "/file").c_str(), (view_ + "/linked").c_str()) == 0);
        return WorkerResult{};
    }));
    owner(raw_ + "/linked", 3, 4);
    struct stat first {}, second {};
    ASSERT_EQ(0, stat((raw_ + "/file").c_str(), &first));
    ASSERT_EQ(0, stat((raw_ + "/linked").c_str(), &second));
    EXPECT_EQ(first.st_ino, second.st_ino); EXPECT_EQ(2u, first.st_nlink);
    EXPECT_EQ(-1, link((raw_ + "/file").c_str(), (view_ + "/cross").c_str()));
    EXPECT_EQ(EXDEV, errno);
    ASSERT_NO_FATAL_FAILURE(raw_file("hole", 150, 4, 0666));
    EXPECT_EQ(-1, link((view_ + "/hole").c_str(), (view_ + "/bad-link").c_str()));
    // Linux linkat calls may_linkat first (EOVERFLOW), before vfs_link's
    // defensive HAS_UNMAPPED_ID check (EPERM).
    EXPECT_EQ(EOVERFLOW, errno);
}

TEST_P(MountIdmapTest, RenameAndExchangeKeepBackingIdsAndInodeIdentity) {
    ASSERT_NO_FATAL_FAILURE(raw_directory("a", 3, 4, 0755));
    ASSERT_NO_FATAL_FAILURE(raw_directory("b", 3, 4, 0755));
    ASSERT_NO_FATAL_FAILURE(raw_file("a/first", 3, 4));
    ASSERT_NO_FATAL_FAILURE(raw_file("b/second", 4, 5));
    struct stat first {}, second {};
    ASSERT_EQ(0, stat((raw_ + "/a/first").c_str(), &first));
    ASSERT_EQ(0, stat((raw_ + "/b/second").c_str(), &second));
    ASSERT_NO_FATAL_FAILURE(as_user(1003, 2004, [&] {
        WORKER_CHECK(syscall(SYS_renameat2, AT_FDCWD, (view_ + "/a/first").c_str(),
                             AT_FDCWD, (view_ + "/b/second").c_str(), kExchange) == 0);
        WORKER_CHECK(rename((view_ + "/b/second").c_str(), (view_ + "/a/moved").c_str()) == 0);
        return WorkerResult{};
    }));
    struct stat moved {}, exchanged {};
    ASSERT_EQ(0, stat((raw_ + "/a/moved").c_str(), &moved));
    ASSERT_EQ(0, stat((raw_ + "/a/first").c_str(), &exchanged));
    EXPECT_EQ(first.st_ino, moved.st_ino); EXPECT_EQ(second.st_ino, exchanged.st_ino);
    owner(raw_ + "/a/moved", 3, 4); owner(raw_ + "/a/first", 4, 5);
}

TEST_P(MountIdmapTest, BindCloneAndNamespaceCopyRetainTheMapping) {
    ASSERT_NO_FATAL_FAILURE(raw_file("file", 17, 23));
    const std::string clone = root_ + "/clone";
    ASSERT_EQ(0, mkdir(clone.c_str(), 0755));
    Fd tree(syscall(SYS_open_tree, AT_FDCWD, view_.c_str(), kClone | O_CLOEXEC));
    ASSERT_GE(tree.get(), 0);
    MountAttr duplicate; duplicate.set = kIdmap; duplicate.userns_fd = namespace_.get();
    EXPECT_EQ(-1, attributes(tree.get(), duplicate)); EXPECT_EQ(EPERM, errno);
    ASSERT_EQ(0, syscall(SYS_move_mount, tree.get(), "", AT_FDCWD, clone.c_str(), kMoveEmpty));
    aliases_.push_back(clone);
    owner(clone + "/file", 1017, 2023); owner(raw_ + "/file", 17, 23);
    ASSERT_EQ(0, unshare(CLONE_NEWNS));
    owner(view_ + "/file", 1017, 2023); owner(clone + "/file", 1017, 2023);
}

TEST_P(MountIdmapTest, DetachedWritableOfdBlocksInstallationButSourceWriterDoesNot) {
    ASSERT_NO_FATAL_FAILURE(raw_file("file", 0, 0, 0666));
    Fd source(open((raw_ + "/file").c_str(), O_RDWR)); ASSERT_GE(source.get(), 0);
    Fd tree(syscall(SYS_open_tree, AT_FDCWD, raw_.c_str(), kClone | O_CLOEXEC));
    ASSERT_GE(tree.get(), 0);
    Fd writer(openat(tree.get(), "file", O_RDWR)); ASSERT_GE(writer.get(), 0);
    Fd viewed(openat(tree.get(), "file", O_RDONLY)); ASSERT_GE(viewed.get(), 0);
    MountAttr attr; attr.set = kIdmap; attr.userns_fd = namespace_.get();
    EXPECT_EQ(-1, attributes(tree.get(), attr)); EXPECT_EQ(EBUSY, errno);
    ASSERT_EQ(0, writer.close_now());
    ASSERT_EQ(0, attributes(tree.get(), attr)) << strerror(errno);
    EXPECT_EQ(1, write(source.get(), "x", 1));
    struct stat st {}; ASSERT_EQ(0, fstat(viewed.get(), &st));
    EXPECT_EQ(1000u, st.st_uid); EXPECT_EQ(2000u, st.st_gid);
}

TEST_P(MountIdmapTest, ReadonlyMappedEntryLeavesRawEntryWritable) {
    ASSERT_NO_FATAL_FAILURE(raw_file("file", 3, 4, 0666));
    Fd path(open(view_.c_str(), O_PATH | O_DIRECTORY)); ASSERT_GE(path.get(), 0);
    MountAttr attr; attr.set = kReadonly;
    ASSERT_EQ(0, attributes(path.get(), attr));
    ASSERT_NO_FATAL_FAILURE(as_user(1003, 2004, [&] {
        Fd denied(open((view_ + "/file").c_str(), O_WRONLY));
        WORKER_CHECK(denied.get() == -1 && errno == EROFS);
        Fd reader(open((view_ + "/file").c_str(), O_RDONLY));
        WORKER_CHECK(reader.get() >= 0);
        struct stat st {};
        WORKER_CHECK(fstat(reader.get(), &st) == 0 && st.st_uid == 1003 && st.st_gid == 2004);
        return WorkerResult{};
    }));
    Fd raw(open((raw_ + "/file").c_str(), O_WRONLY)); ASSERT_GE(raw.get(), 0);
    EXPECT_EQ(1, write(raw.get(), "x", 1));
    owner(raw_ + "/file", 3, 4);
}

TEST_P(MountIdmapTest, MappingRetainsDetachedWriterUntilMunmap) {
    ASSERT_NO_FATAL_FAILURE(raw_file("file", 0, 0, 0666));
    Fd tree(syscall(SYS_open_tree, AT_FDCWD, raw_.c_str(), kClone | O_CLOEXEC));
    ASSERT_GE(tree.get(), 0);
    Fd writer(openat(tree.get(), "file", O_RDWR)); ASSERT_GE(writer.get(), 0);
    const long page = sysconf(_SC_PAGESIZE); ASSERT_GT(page, 0);
    ASSERT_EQ(0, ftruncate(writer.get(), page));
    void* mapping = mmap(nullptr, page, PROT_READ | PROT_WRITE, MAP_SHARED, writer.get(), 0);
    ASSERT_NE(MAP_FAILED, mapping);
    ASSERT_EQ(0, writer.close_now());
    MountAttr attr; attr.set = kIdmap; attr.userns_fd = namespace_.get();
    EXPECT_EQ(-1, attributes(tree.get(), attr)); EXPECT_EQ(EBUSY, errno);
    static_cast<char*>(mapping)[0] = 'x';
    ASSERT_EQ(0, munmap(mapping, page));
    ASSERT_EQ(0, attributes(tree.get(), attr));
}

TEST_P(MountIdmapTest, FallocateAndTruncateKillSetidWithoutWritingViewOwnerBack) {
    ASSERT_NO_FATAL_FAILURE(raw_file("allocate", 3, 4, 06755));
    ASSERT_NO_FATAL_FAILURE(raw_file("truncate", 3, 4, 06755));
    ASSERT_NO_FATAL_FAILURE(as_user(1003, 2004, [&] {
        Fd allocate(open((view_ + "/allocate").c_str(), O_RDWR));
        WORKER_CHECK(allocate.get() >= 0);
        WORKER_CHECK(fallocate(allocate.get(), 0, 0, 4096) == 0);
        Fd truncate(open((view_ + "/truncate").c_str(), O_RDWR));
        WORKER_CHECK(truncate.get() >= 0);
        WORKER_CHECK(ftruncate(truncate.get(), 8192) == 0);
        struct stat st {};
        WORKER_CHECK(fstat(allocate.get(), &st) == 0 && st.st_uid == 1003 && st.st_gid == 2004);
        return WorkerResult{};
    }));
    for (const char* name : {"allocate", "truncate"}) {
        owner(raw_ + "/" + name, 3, 4);
        struct stat st {}; ASSERT_EQ(0, stat((raw_ + "/" + name).c_str(), &st));
        EXPECT_EQ(0u, st.st_mode & (S_ISUID | S_ISGID));
    }
}

TEST_P(MountIdmapTest, MaskedChmodCannotRaceRawChownIntoPersistingViewIds) {
    ASSERT_NO_FATAL_FAILURE(raw_file("file", 3, 4));
    std::atomic<bool> start{false};
    std::atomic<int> error{0};
    auto wait_start = [&] { while (!start.load(std::memory_order_acquire)) std::this_thread::yield(); };
    std::thread chowner([&] {
        wait_start();
        for (int i = 0; i < 48; ++i)
            if (chown((raw_ + "/file").c_str(), i % 2 ? 3 : 5, i % 2 ? 4 : 6) != 0)
                error.store(errno);
    });
    std::thread chmodder([&] {
        wait_start();
        for (int i = 0; i < 48; ++i)
            if (chmod((view_ + "/file").c_str(), i % 2 ? 0600 : 0644) != 0)
                error.store(errno);
    });
    start.store(true, std::memory_order_release);
    for (int i = 0; i < 96; ++i) {
        struct stat st {};
        if (stat((raw_ + "/file").c_str(), &st) != 0) error.store(errno);
        else if (!((st.st_uid == 3 && st.st_gid == 4) || (st.st_uid == 5 && st.st_gid == 6)))
            error.store(EOVERFLOW);
        std::this_thread::yield();
    }
    chowner.join(); chmodder.join();
    EXPECT_EQ(0, error.load()) << strerror(error.load());
}

TEST_P(MountIdmapTest, UntouchedSameInodeMappingCanSupplyPwriteWithoutLockInversion) {
    ASSERT_NO_FATAL_FAILURE(raw_file("file", 3, 4, 06755));
    const long page = sysconf(_SC_PAGESIZE); ASSERT_GT(page, 0);
    ASSERT_NO_FATAL_FAILURE(as_user(1003, 2004, [&] {
        Fd fd(open((view_ + "/file").c_str(), O_RDWR));
        WORKER_CHECK(fd.get() >= 0);
        void* source = mmap(nullptr, page, PROT_READ, MAP_SHARED, fd.get(), 0);
        WORKER_CHECK(source != MAP_FAILED);
        // Do not read source or populate this VMA before pwrite: copying the
        // source must take a file-backed fault on this very destination inode.
        const ssize_t written = pwrite(fd.get(), source, 4, page);
        const int write_error = errno;
        WORKER_CHECK(munmap(source, page) == 0);
        errno = write_error;
        WORKER_CHECK(written == 4);
        char original[4], copied[4];
        WORKER_CHECK(pread(fd.get(), original, sizeof(original), 0) == 4);
        WORKER_CHECK(pread(fd.get(), copied, sizeof(copied), page) == 4);
        WORKER_CHECK(memcmp(original, "data", 4) == 0 && memcmp(copied, "data", 4) == 0);
        struct stat st {};
        WORKER_CHECK(fstat(fd.get(), &st) == 0 && st.st_uid == 1003 && st.st_gid == 2004);
        WORKER_CHECK(st.st_size == page + 4 && (st.st_mode & (S_ISUID | S_ISGID)) == 0);
        return WorkerResult{};
    }));
    owner(raw_ + "/file", 3, 4);
    Fd raw(open((raw_ + "/file").c_str(), O_RDONLY)); ASSERT_GE(raw.get(), 0);
    char copied[4]; ASSERT_EQ(4, pread(raw.get(), copied, sizeof(copied), page));
    EXPECT_EQ(0, memcmp(copied, "data", 4));
    struct stat st {}; ASSERT_EQ(0, fstat(raw.get(), &st));
    EXPECT_EQ(page + 4, st.st_size);
    EXPECT_EQ(0u, st.st_mode & (S_ISUID | S_ISGID));
}

TEST_P(MountIdmapTest, ModeRemovalNotifiesBeforeMappedDataWrite) {
    ASSERT_NO_FATAL_FAILURE(raw_file("file", 3, 4, 06755));
    Fd notify(inotify_init1(IN_NONBLOCK | IN_CLOEXEC)); ASSERT_GE(notify.get(), 0);
    ASSERT_GE(inotify_add_watch(notify.get(), (raw_ + "/file").c_str(), IN_ATTRIB | IN_MODIFY), 0);
    ASSERT_NO_FATAL_FAILURE(as_user(1003, 2004, [&] {
        Fd fd(open((view_ + "/file").c_str(), O_WRONLY));
        WORKER_CHECK(fd.get() >= 0 && write(fd.get(), "x", 1) == 1);
        return WorkerResult{};
    }));
    int attrib = -1, modify = -1;
    const auto masks = event_masks(notify.get());
    for (size_t i = 0; i < masks.size(); ++i) {
        if (attrib < 0 && (masks[i] & IN_ATTRIB)) attrib = static_cast<int>(i);
        if (modify < 0 && (masks[i] & IN_MODIFY)) modify = static_cast<int>(i);
    }
    EXPECT_GE(attrib, 0); EXPECT_GT(modify, attrib);
    owner(raw_ + "/file", 3, 4);
    struct stat st {}; ASSERT_EQ(0, stat((raw_ + "/file").c_str(), &st));
    EXPECT_EQ(0u, st.st_mode & (S_ISUID | S_ISGID));
}

TEST_P(MountIdmapTest, DataFaultAfterPrivilegeStageStillNotifiesAttribWithoutModify) {
    ASSERT_NO_FATAL_FAILURE(raw_file("file", 3, 4, 06755));
    const long page = sysconf(_SC_PAGESIZE); ASSERT_GT(page, 0);
    // Keep surrounding address space reserved, but remove the middle page:
    // access_ok succeeds on a valid user-range pointer; actual copying faults.
    // This is intentionally different from a pointer outside user range.
    void* reservation = mmap(nullptr, page * 3, PROT_NONE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, reservation);
    char* hole = static_cast<char*>(reservation) + page;
    ASSERT_EQ(0, munmap(hole, page));
    Fd notify(inotify_init1(IN_NONBLOCK | IN_CLOEXEC)); ASSERT_GE(notify.get(), 0);
    ASSERT_GE(inotify_add_watch(notify.get(), (raw_ + "/file").c_str(), IN_ATTRIB | IN_MODIFY), 0);
    ASSERT_NO_FATAL_FAILURE(as_user(1003, 2004, [&] {
        Fd fd(open((view_ + "/file").c_str(), O_WRONLY));
        WORKER_CHECK(fd.get() >= 0);
        WORKER_CHECK(syscall(SYS_write, fd.get(), hole, 1) == -1 && errno == EFAULT);
        return WorkerResult{};
    }));
    bool attrib = false, modify = false;
    for (const auto mask : event_masks(notify.get())) {
        attrib |= (mask & IN_ATTRIB) != 0;
        modify |= (mask & IN_MODIFY) != 0;
    }
    EXPECT_TRUE(attrib); EXPECT_FALSE(modify);
    owner(raw_ + "/file", 3, 4);
    struct stat st {}; ASSERT_EQ(0, stat((raw_ + "/file").c_str(), &st));
    EXPECT_EQ(0u, st.st_mode & (S_ISUID | S_ISGID));
    EXPECT_EQ(4, st.st_size);
    EXPECT_EQ(0, munmap(reservation, page));
    EXPECT_EQ(0, munmap(static_cast<char*>(reservation) + page * 2, page));
}

TEST_P(MountIdmapTest, LateOwnerHoleSeparatesFallocateFromTruncateAndPrivilegeStage) {
    for (bool setid : {false, true}) {
        for (int flags : {0, FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE}) {
            const std::string name = "late_" + std::to_string(setid) + "_" + std::to_string(flags);
            ASSERT_NO_FATAL_FAILURE(raw_file(name.c_str(), 3, 4, 0666));
            int ready[2], resume[2];
            ASSERT_EQ(0, pipe(ready));
            Fd ready_read(ready[0]), ready_write(ready[1]);
            ASSERT_EQ(0, pipe(resume));
            Fd resume_read(resume[0]), resume_write(resume[1]);
            ASSERT_NO_FATAL_FAILURE(as_user_with_parent(1003, 2004, [&] {
                ready_read.close_now(); resume_write.close_now();
                Fd fd(open((view_ + "/" + name).c_str(), O_RDWR));
                WORKER_CHECK(fd.get() >= 0);
                char ack = 0;
                WORKER_CHECK(write(ready_write.get(), &ack, 1) == 1);
                WORKER_CHECK(read(resume_read.get(), &ack, 1) == 1);
                if (setid) {
                    WORKER_CHECK(pwrite(fd.get(), "x", 1, 0) == -1 && errno == EOVERFLOW);
                }
                const int rc = fallocate(fd.get(), flags, 0, 8192);
                if (setid && GetParam()) WORKER_CHECK(rc == -1 && errno == EOVERFLOW);
                else WORKER_CHECK(rc == 0);
                struct stat st {};
                WORKER_CHECK(fstat(fd.get(), &st) == 0 && st.st_uid == 65534 && st.st_gid == 65534);
                const off_t size = flags == 0 && !(setid && GetParam()) ? 8192 : 4;
                WORKER_CHECK(st.st_size == size);
                WORKER_CHECK(ftruncate(fd.get(), 16384) == -1 && errno == EOVERFLOW);
                WORKER_CHECK(fstat(fd.get(), &st) == 0 && st.st_size == size);
                if (setid) WORKER_CHECK((st.st_mode & S_ISUID) != 0);
                return WorkerResult{};
            }, [&] {
                ready_write.close_now(); resume_read.close_now();
                char ack = 0;
                WORKER_CHECK(read(ready_read.get(), &ack, 1) == 1);
                WorkerResult result;
                if (chown((raw_ + "/" + name).c_str(), 150, 160) != 0 ||
                    (setid && chmod((raw_ + "/" + name).c_str(), 04666) != 0))
                    result = {__LINE__, errno};
                if (write(resume_write.get(), &ack, 1) != 1) return WorkerResult{__LINE__, errno};
                return result;
            }));
            owner(raw_ + "/" + name, 150, 160);
        }
    }
}

class MountIdmapExt4CapsTest : public MountIdmapTest {};

class MountIdmapTmpfsQuotaTest : public MountIdmapTest {};

TEST_P(MountIdmapTmpfsQuotaTest, QuotaFailureAfterPrivilegeStageNotifiesOnlyAttrib) {
    const long page = sysconf(_SC_PAGESIZE); ASSERT_GT(page, 0);
    const std::string options = "size=" + std::to_string(page);
    ASSERT_EQ(0, mount(nullptr, raw_.c_str(), "tmpfs", MS_REMOUNT, options.c_str()));
    const std::string target = raw_ + "/empty";
    {
        Fd fd(open(target.c_str(), O_CREAT | O_EXCL | O_WRONLY, 0600));
        ASSERT_GE(fd.get(), 0);
    }
    ASSERT_EQ(0, chown(target.c_str(), 3, 4));
    ASSERT_EQ(0, chmod(target.c_str(), 06755));
    {
        Fd filler(open((raw_ + "/filler").c_str(), O_CREAT | O_EXCL | O_WRONLY, 0600));
        ASSERT_GE(filler.get(), 0);
        std::vector<char> bytes(page, 'x');
        ASSERT_EQ(page, write(filler.get(), bytes.data(), bytes.size()));
    }
    Fd notify(inotify_init1(IN_NONBLOCK | IN_CLOEXEC)); ASSERT_GE(notify.get(), 0);
    ASSERT_GE(inotify_add_watch(notify.get(), target.c_str(), IN_ATTRIB | IN_MODIFY), 0);
    ASSERT_NO_FATAL_FAILURE(as_user(1003, 2004, [&] {
        Fd fd(open((view_ + "/empty").c_str(), O_WRONLY));
        WORKER_CHECK(fd.get() >= 0);
        WORKER_CHECK(write(fd.get(), "x", 1) == -1 && errno == ENOSPC);
        return WorkerResult{};
    }));
    bool attrib = false, modify = false;
    for (uint32_t mask : event_masks(notify.get())) {
        attrib |= (mask & IN_ATTRIB) != 0;
        modify |= (mask & IN_MODIFY) != 0;
    }
    EXPECT_TRUE(attrib); EXPECT_FALSE(modify);
    struct stat st {}; ASSERT_EQ(0, stat(target.c_str(), &st));
    EXPECT_EQ(0u, st.st_mode & (S_ISUID | S_ISGID));
    EXPECT_EQ(0, st.st_size);
    owner(target, 3, 4);
}

TEST_P(MountIdmapExt4CapsTest, FileCapabilitiesTranslateRealMountRootAndInternalRemoval) {
    struct CapsV2 { uint32_t magic; uint32_t words[4]; };
    struct CapsV3 { CapsV2 caps; uint32_t root; };
    static_assert(sizeof(CapsV2) == 20 && sizeof(CapsV3) == 24);
    ASSERT_NO_FATAL_FAILURE(raw_file("caps", 3, 4, 0666));
    const std::string raw = raw_ + "/caps", view = view_ + "/caps";
    CapsV3 root_zero {{0x03000001, {1, 0, 0, 0}}, 0};
    ASSERT_EQ(0, setxattr(raw.c_str(), "security.capability", &root_zero, sizeof(root_zero), 0));
    CapsV2 raw_caps {};
    ASSERT_EQ(20, getxattr(raw.c_str(), "security.capability", &raw_caps, sizeof(raw_caps)));
    EXPECT_EQ(0x02000001u, raw_caps.magic);
    EXPECT_EQ(1u, raw_caps.words[0]);
    CapsV3 mapped {};
    ASSERT_EQ(24, getxattr(view.c_str(), "security.capability", &mapped, sizeof(mapped)));
    EXPECT_EQ(0x03000001u, mapped.caps.magic);
    EXPECT_EQ(1000u, mapped.root);
    EXPECT_EQ(1u, mapped.caps.words[0]);
    ASSERT_EQ(24, getxattr(view.c_str(), "security.capability", nullptr, 0));
    ASSERT_EQ(-1, getxattr(view.c_str(), "security.capability", &mapped, 23));
    ASSERT_EQ(ERANGE, errno);
    mapped = {{0x03000001, {1, 0, 0, 0}}, 1000};
    ASSERT_EQ(0, setxattr(view.c_str(), "security.capability", &mapped, sizeof(mapped), 0));
    ASSERT_EQ(20, getxattr(raw.c_str(), "security.capability", &raw_caps, sizeof(raw_caps)));
    EXPECT_EQ(0x02000001u, raw_caps.magic);
    EXPECT_EQ(1u, raw_caps.words[0]);
    ASSERT_NO_FATAL_FAILURE(as_user(1003, 2004, [&] {
        WORKER_CHECK(removexattr(view.c_str(), "security.capability") == -1 && errno == EPERM);
        Fd fd(open(view.c_str(), O_WRONLY));
        WORKER_CHECK(fd.get() >= 0 && write(fd.get(), "x", 1) == 1);
        return WorkerResult{};
    }));
    for (const auto& path : {raw, view}) {
        EXPECT_EQ(-1, getxattr(path.c_str(), "security.capability", nullptr, 0));
        EXPECT_EQ(ENODATA, errno);
    }
    owner(raw, 3, 4);
}

INSTANTIATE_TEST_SUITE_P(Ext4, MountIdmapExt4CapsTest, ::testing::Values(true));
INSTANTIATE_TEST_SUITE_P(Tmpfs, MountIdmapTmpfsQuotaTest, ::testing::Values(false));

INSTANTIATE_TEST_SUITE_P(Backends, MountIdmapTest, ::testing::Values(false, true),
    [](const ::testing::TestParamInfo<bool>& info) { return info.param ? "Ext4" : "Tmpfs"; });

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
