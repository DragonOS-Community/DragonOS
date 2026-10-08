#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>
#include <vector>

#ifndef SYS_open_tree
#define SYS_open_tree 428
#endif
#ifndef SYS_mount_setattr
#define SYS_mount_setattr 442
#endif

namespace {

constexpr unsigned kRecursive = 0x8000;
constexpr unsigned kEmptyPath = 0x1000;
constexpr unsigned kNoAutomount = 0x800;
constexpr unsigned kClone = 1;
constexpr uint64_t kReadonly = 1, kNosuid = 2, kNoexec = 8;
constexpr uint64_t kNoatime = 0x10, kStrictAtime = 0x20, kAtime = 0x70;
constexpr uint64_t kNodiratime = 0x80, kIdmap = 0x100000;

struct MountAttr {
    uint64_t set = 0, clear = 0, propagation = 0, userns_fd = 0;
};
static_assert(sizeof(MountAttr) == 32);

class Fd {
public:
    explicit Fd(int fd) : fd_(fd) {}
    ~Fd() { if (fd_ >= 0) close(fd_); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return fd_; }
    int close_now() { const int fd = fd_; fd_ = -1; return close(fd); }
private:
    int fd_;
};

int setattr(int dfd, const char* path, unsigned flags, const void* attr,
            size_t size = sizeof(MountAttr)) {
    return syscall(SYS_mount_setattr, dfd, path, flags, attr, size);
}

// Return exact options/optional-field tokens for the selected mount, not
// substrings or superblock flags combined from an unrelated bind entry.
std::vector<std::string> mount_tokens(const std::string& path, bool optional = false) {
    std::vector<std::string> result;
    FILE* file = fopen(optional ? "/proc/self/mountinfo" : "/proc/self/mounts", "r");
    if (!file) {
        ADD_FAILURE() << strerror(errno);
        return result;
    }
    char* line = nullptr;
    size_t capacity = 0;
    bool found = false;
    while (getline(&line, &capacity, file) >= 0) {
        char* save = nullptr;
        std::vector<std::string> fields;
        for (char* item = strtok_r(line, " \n", &save); item;
             item = strtok_r(nullptr, " \n", &save)) fields.emplace_back(item);
        const size_t target = optional ? 4 : 1;
        if (fields.size() <= target || fields[target] != path) continue;
        found = true;
        if (optional) {
            for (size_t i = 6; i < fields.size() && fields[i] != "-"; ++i)
                result.push_back(fields[i]);
        } else if (fields.size() > 3) {
            std::string options = fields[3];
            char* option_save = nullptr;
            for (char* item = strtok_r(options.data(), ",", &option_save); item;
                 item = strtok_r(nullptr, ",", &option_save)) result.emplace_back(item);
        }
        break;
    }
    free(line);
    fclose(file);
    EXPECT_TRUE(found) << path;
    return result;
}

bool option(const std::string& path, const char* wanted) {
    for (const auto& item : mount_tokens(path)) if (item == wanted) return true;
    return false;
}

bool shared(const std::string& path) {
    for (const auto& item : mount_tokens(path, true))
        if (item.compare(0, 7, "shared:") == 0) return true;
    return false;
}

// Retain a noninitial user namespace without changing this test process's
// credentials or mount-owner authority. Child failure is an assertion, not a
// skip for an unimplemented namespace interface.
int user_namespace_fd() {
    int ready[2], release[2];
    if (pipe(ready) != 0) return -1;
    if (pipe(release) != 0) { close(ready[0]); close(ready[1]); return -1; }
    const pid_t child = fork();
    if (child < 0) {
        close(ready[0]); close(ready[1]); close(release[0]); close(release[1]);
        return -1;
    }
    if (child == 0) {
        close(ready[0]); close(release[1]);
        const int status = unshare(CLONE_NEWUSER) == 0 ? 0 : errno;
        if (write(ready[1], &status, sizeof(status)) != sizeof(status)) _exit(2);
        char ack;
        if (read(release[0], &ack, 1) != 1) _exit(3);
        _exit(0);
    }
    close(ready[1]); close(release[0]);
    int status = EIO;
    const bool notified = read(ready[0], &status, sizeof(status)) == sizeof(status);
    const std::string path = "/proc/" + std::to_string(child) + "/ns/user";
    const int fd = notified && status == 0 ? open(path.c_str(), O_RDONLY | O_CLOEXEC) : -1;
    const int saved = fd >= 0 ? 0 : (notified && status != 0 ? status : errno);
    const char ack = 0;
    EXPECT_EQ(1, write(release[1], &ack, 1));
    close(ready[0]); close(release[1]);
    int exit_status = 0;
    EXPECT_EQ(child, waitpid(child, &exit_status, 0));
    EXPECT_TRUE(WIFEXITED(exit_status) && WEXITSTATUS(exit_status) == 0);
    errno = saved;
    return fd;
}

class MountAttributesTest : public ::testing::Test {
protected:
    void SetUp() override {
        ASSERT_EQ(0, unshare(CLONE_NEWNS)) << strerror(errno);
        ASSERT_EQ(0, mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr))
            << strerror(errno);
        struct stat st {};
        if (stat("/tmp", &st) != 0) {
            ASSERT_EQ(0, mkdir("/tmp", 0777));
        }
        char name[] = "/tmp/mount_attributes_XXXXXX";
        char* directory = mkdtemp(name);
        ASSERT_NE(nullptr, directory) << strerror(errno);
        root_ = directory;
        src_ = root_ + "/src";
        alias_ = root_ + "/alias";
        ASSERT_EQ(0, mkdir(src_.c_str(), 0755));
        ASSERT_EQ(0, mkdir(alias_.c_str(), 0755));
        ASSERT_EQ(0, mount("mount_attributes", src_.c_str(), "tmpfs", 0, "mode=0755"))
            << strerror(errno);
        mounts_.push_back(src_);
    }

    void TearDown() override {
        for (auto it = mounts_.rbegin(); it != mounts_.rend(); ++it)
            if (umount2(it->c_str(), MNT_DETACH) != 0 && errno != EINVAL && errno != ENOENT)
                ADD_FAILURE() << *it << ": " << strerror(errno);
        if (!root_.empty()) {
            unlink((root_ + "/link").c_str());
            rmdir(alias_.c_str()); rmdir(src_.c_str()); rmdir(root_.c_str());
        }
    }

    void child_mount() {
        child_ = src_ + "/child";
        ASSERT_EQ(0, mkdir(child_.c_str(), 0755));
        ASSERT_EQ(0, mount("child", child_.c_str(), "tmpfs", 0, nullptr)) << strerror(errno);
        mounts_.push_back(child_);
    }

    int apply(const MountAttr& attr, unsigned flags = 0) {
        return setattr(AT_FDCWD, src_.c_str(), flags, &attr);
    }

    std::string root_, src_, alias_, child_;
    std::vector<std::string> mounts_;
};

TEST_F(MountAttributesTest, FlagsSizeAndNoopPrecedence) {
    MountAttr noop;
    noop.userns_fd = UINT64_MAX;
    const char* bad_path = reinterpret_cast<const char*>(1);
    EXPECT_EQ(0, setattr(-1, bad_path, 0, &noop));
    EXPECT_EQ(0, setattr(-1, bad_path, kRecursive | kNoAutomount, &noop));
    EXPECT_EQ(-1, setattr(-1, bad_path, 0x40000000, nullptr, 8192));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(-1, setattr(-1, bad_path, 0, nullptr, 8192));
    EXPECT_EQ(E2BIG, errno);
    EXPECT_EQ(-1, setattr(-1, bad_path, 0, nullptr, 31));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(-1, setattr(-1, bad_path, 0, nullptr));
    EXPECT_EQ(EFAULT, errno);
    MountAttr invalid;
    invalid.set = UINT64_C(1) << 63;
    EXPECT_EQ(-1, setattr(-1, bad_path, 0, &invalid));
    EXPECT_EQ(EINVAL, errno);
}

TEST_F(MountAttributesTest, ExtensionStopsAtNonzeroBeforeLaterUnmappedPage) {
    const long page = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page, 128);
    void* mapping = mmap(nullptr, page * 2, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping) << strerror(errno);
    char* attr = static_cast<char*>(mapping) + page - 64;
    memset(attr, 0, 64);
    ASSERT_EQ(0, mprotect(static_cast<char*>(mapping) + page, page, PROT_NONE));
    uint64_t nonzero = 1;
    memcpy(attr + 32, &nonzero, sizeof(nonzero));
    EXPECT_EQ(-1, setattr(-1, reinterpret_cast<const char*>(1), 0, attr, 128));
    EXPECT_EQ(E2BIG, errno);
    memset(attr + 32, 0, 32);
    EXPECT_EQ(-1, setattr(-1, reinterpret_cast<const char*>(1), 0, attr, 128));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_EQ(0, setattr(-1, reinterpret_cast<const char*>(1), 0, attr, 64));
    EXPECT_EQ(0, munmap(mapping, page * 2));
}

TEST_F(MountAttributesTest, AtimeEnumReplacesWholeGroupAndPreservesNodiratime) {
    MountAttr attr;
    attr.set = kNoatime | kNodiratime; attr.clear = kAtime;
    ASSERT_EQ(0, apply(attr)) << strerror(errno);
    EXPECT_TRUE(option(src_, "noatime")); EXPECT_TRUE(option(src_, "nodiratime"));
    attr.set = kStrictAtime;
    ASSERT_EQ(0, apply(attr));
    EXPECT_FALSE(option(src_, "noatime")); EXPECT_FALSE(option(src_, "relatime"));
    EXPECT_TRUE(option(src_, "nodiratime"));
    attr.set = 0;
    ASSERT_EQ(0, apply(attr));
    EXPECT_TRUE(option(src_, "relatime")); EXPECT_TRUE(option(src_, "nodiratime"));
    attr.set = kNoatime; attr.clear = 0;
    EXPECT_EQ(-1, apply(attr)); EXPECT_EQ(EINVAL, errno);
    attr.clear = kNoatime;
    EXPECT_EQ(-1, apply(attr)); EXPECT_EQ(EINVAL, errno);
    attr.clear = kAtime; attr.set = kNoatime | kStrictAtime;
    EXPECT_EQ(-1, apply(attr)); EXPECT_EQ(EINVAL, errno);
    EXPECT_TRUE(option(src_, "relatime")); EXPECT_TRUE(option(src_, "nodiratime"));
}

TEST_F(MountAttributesTest, RootEmptyPathAndNoAutomountFollowExistingMount) {
    const std::string below = src_ + "/below";
    ASSERT_EQ(0, mkdir(below.c_str(), 0755));
    MountAttr attr; attr.set = kNosuid;
    EXPECT_EQ(-1, setattr(AT_FDCWD, below.c_str(), 0, &attr)); EXPECT_EQ(EINVAL, errno);
    const std::string link = root_ + "/link";
    ASSERT_EQ(0, symlink(src_.c_str(), link.c_str()));
    EXPECT_EQ(-1, setattr(AT_FDCWD, link.c_str(), 0x100, &attr)); EXPECT_EQ(EINVAL, errno);
    ASSERT_EQ(0, setattr(AT_FDCWD, link.c_str(), kNoAutomount, &attr));
    EXPECT_TRUE(option(src_, "nosuid"));
    Fd fd(open(src_.c_str(), O_PATH | O_DIRECTORY));
    ASSERT_GE(fd.get(), 0);
    attr.set = kNoexec;
    ASSERT_EQ(0, setattr(fd.get(), "", kEmptyPath, &attr));
    EXPECT_TRUE(option(src_, "noexec"));
    EXPECT_EQ(-1, setattr(fd.get(), "", 0, &attr)); EXPECT_EQ(ENOENT, errno);
}

TEST_F(MountAttributesTest, ActiveWriterAllowsNosuidNoexecButBlocksReadonly) {
    const std::string file = src_ + "/file";
    Fd writer(open(file.c_str(), O_CREAT | O_RDWR, 0644));
    ASSERT_GE(writer.get(), 0);
    const std::string executable = src_ + "/executable";
    {
        Fd script(open(executable.c_str(), O_CREAT | O_WRONLY, 0755));
        ASSERT_GE(script.get(), 0);
        const char content[] = "#!/bin/sh\nexit 37\n";
        ASSERT_EQ(static_cast<ssize_t>(sizeof(content) - 1),
                  write(script.get(), content, sizeof(content) - 1));
    }
    MountAttr attr; attr.set = kNosuid | kNoexec;
    ASSERT_EQ(0, apply(attr)) << strerror(errno);
    EXPECT_TRUE(option(src_, "nosuid")); EXPECT_TRUE(option(src_, "noexec"));
    EXPECT_EQ(1, write(writer.get(), "x", 1));
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        execl(executable.c_str(), executable.c_str(), static_cast<char*>(nullptr));
        _exit(errno == EACCES ? 0 : 99);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    EXPECT_TRUE(WIFEXITED(status) && WEXITSTATUS(status) == 0)
        << "NOEXEC must reject an executable inode, not just appear in procfs";
    attr.set = kReadonly;
    EXPECT_EQ(-1, apply(attr)); EXPECT_EQ(EBUSY, errno);
    EXPECT_TRUE(option(src_, "rw"));
    ASSERT_EQ(0, writer.close_now());
    ASSERT_EQ(0, apply(attr));
    EXPECT_TRUE(option(src_, "ro"));
    EXPECT_EQ(-1, open(file.c_str(), O_WRONLY)); EXPECT_EQ(EROFS, errno);
}

TEST_F(MountAttributesTest, RecursiveBusyRollsBackReadonlyAndPreparedSharedGraph) {
    child_mount(); ASSERT_FALSE(HasFatalFailure());
    const std::string file = child_ + "/file";
    Fd writer(open(file.c_str(), O_CREAT | O_RDWR, 0644));
    ASSERT_GE(writer.get(), 0);
    MountAttr attr; attr.set = kReadonly; attr.propagation = MS_SHARED;
    EXPECT_EQ(-1, apply(attr, kRecursive)); EXPECT_EQ(EBUSY, errno);
    EXPECT_TRUE(option(src_, "rw")); EXPECT_TRUE(option(child_, "rw"));
    EXPECT_FALSE(shared(src_)); EXPECT_FALSE(shared(child_));
    // A released first-target hold must not prevent a new open after abort.
    { Fd other(open((src_ + "/other").c_str(), O_CREAT | O_RDWR, 0644));
      ASSERT_GE(other.get(), 0) << strerror(errno); }
    ASSERT_EQ(0, writer.close_now());
    ASSERT_EQ(0, apply(attr, kRecursive)) << strerror(errno);
    EXPECT_TRUE(option(src_, "ro")); EXPECT_TRUE(option(child_, "ro"));
    EXPECT_TRUE(shared(src_)); EXPECT_TRUE(shared(child_));
}

TEST_F(MountAttributesTest, ReadonlyChangesOnlySelectedBindEntryNotSuperblock) {
    ASSERT_EQ(0, mount(src_.c_str(), alias_.c_str(), nullptr, MS_BIND, nullptr));
    mounts_.push_back(alias_);
    const std::string file = src_ + "/file";
    Fd writer(open(file.c_str(), O_CREAT | O_RDWR, 0644));
    ASSERT_GE(writer.get(), 0);
    MountAttr attr; attr.set = kReadonly;
    ASSERT_EQ(0, setattr(AT_FDCWD, alias_.c_str(), 0, &attr)) << strerror(errno);
    EXPECT_TRUE(option(alias_, "ro")); EXPECT_TRUE(option(src_, "rw"));
    EXPECT_EQ(1, write(writer.get(), "x", 1));
    EXPECT_EQ(-1, open((alias_ + "/file").c_str(), O_WRONLY)); EXPECT_EQ(EROFS, errno);
    attr.clear = kReadonly; attr.set = 0;
    ASSERT_EQ(0, setattr(AT_FDCWD, alias_.c_str(), 0, &attr));
    Fd again(open((alias_ + "/file").c_str(), O_WRONLY));
    EXPECT_GE(again.get(), 0) << strerror(errno);
}

TEST_F(MountAttributesTest, SharedWritableMappingRetainsWriterAfterFdClose) {
    Fd writer(open((src_ + "/mapped").c_str(), O_CREAT | O_RDWR, 0644));
    ASSERT_GE(writer.get(), 0);
    const long page = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page, 0);
    ASSERT_EQ(0, ftruncate(writer.get(), page));
    void* mapping = mmap(nullptr, page, PROT_READ | PROT_WRITE, MAP_SHARED, writer.get(), 0);
    ASSERT_NE(MAP_FAILED, mapping) << strerror(errno);
    ASSERT_EQ(0, writer.close_now());
    MountAttr attr; attr.set = kReadonly;
    EXPECT_EQ(-1, apply(attr)); EXPECT_EQ(EBUSY, errno);
    EXPECT_TRUE(option(src_, "rw"));
    static_cast<char*>(mapping)[0] = 'x';
    ASSERT_EQ(0, munmap(mapping, page));
    ASSERT_EQ(0, apply(attr)) << strerror(errno);
    EXPECT_TRUE(option(src_, "ro"));
}

TEST_F(MountAttributesTest, OldFdCannotReconfigureAnotherMountNamespace) {
    Fd old(open(src_.c_str(), O_PATH | O_DIRECTORY)); ASSERT_GE(old.get(), 0);
    ASSERT_EQ(0, unshare(CLONE_NEWNS)) << strerror(errno);
    MountAttr attr; attr.set = kNoexec;
    EXPECT_EQ(-1, setattr(old.get(), "", kEmptyPath, &attr)); EXPECT_EQ(EINVAL, errno);
    EXPECT_FALSE(option(src_, "noexec"));
    ASSERT_EQ(0, apply(attr));
    EXPECT_TRUE(option(src_, "noexec"));
}

TEST_F(MountAttributesTest, DetachedRootAllowedButAnonymousChildRejected) {
    child_mount(); ASSERT_FALSE(HasFatalFailure());
    Fd tree(syscall(SYS_open_tree, AT_FDCWD, src_.c_str(), kClone | kRecursive | O_CLOEXEC));
    ASSERT_GE(tree.get(), 0) << strerror(errno);
    Fd child(openat(tree.get(), "child", O_PATH | O_DIRECTORY));
    ASSERT_GE(child.get(), 0) << strerror(errno);
    MountAttr attr; attr.set = kNoexec;
    ASSERT_EQ(0, setattr(tree.get(), "", kEmptyPath, &attr)) << strerror(errno);
    EXPECT_EQ(-1, setattr(child.get(), "", kEmptyPath, &attr)); EXPECT_EQ(EINVAL, errno);
    EXPECT_FALSE(option(src_, "noexec")); EXPECT_FALSE(option(child_, "noexec"));
}

TEST_F(MountAttributesTest, InvalidPropagationDoesNotChangeAttributes) {
    MountAttr attr; attr.set = kNoexec; attr.propagation = MS_SHARED | MS_PRIVATE;
    EXPECT_EQ(-1, apply(attr)); EXPECT_EQ(EINVAL, errno);
    EXPECT_FALSE(option(src_, "noexec")); EXPECT_FALSE(shared(src_));
    attr.propagation = UINT64_C(1) << 63;
    EXPECT_EQ(-1, apply(attr)); EXPECT_EQ(EINVAL, errno);
    EXPECT_FALSE(option(src_, "noexec"));
}

TEST_F(MountAttributesTest, IdmapFdValidationPrecedesPathLookup) {
    MountAttr attr; attr.set = kIdmap;
    const char* bad = reinterpret_cast<const char*>(1);
    attr.userns_fd = UINT64_MAX;
    EXPECT_EQ(-1, setattr(-1, bad, 0, &attr)); EXPECT_EQ(EINVAL, errno);
    attr.userns_fd = INT32_MAX;
    EXPECT_EQ(-1, setattr(-1, bad, 0, &attr)); EXPECT_EQ(EBADF, errno);
    Fd ordinary(open((src_ + "/ordinary").c_str(), O_CREAT | O_RDONLY, 0644));
    ASSERT_GE(ordinary.get(), 0);
    attr.userns_fd = ordinary.get();
    EXPECT_EQ(-1, setattr(-1, bad, 0, &attr)); EXPECT_EQ(EINVAL, errno);
    Fd initial(open("/proc/self/ns/user", O_RDONLY)); ASSERT_GE(initial.get(), 0);
    attr.userns_fd = initial.get();
    EXPECT_EQ(-1, setattr(-1, bad, 0, &attr)); EXPECT_EQ(EPERM, errno);
    attr.set = 0; attr.clear = kIdmap;
    EXPECT_EQ(-1, setattr(-1, bad, 0, &attr)); EXPECT_EQ(EINVAL, errno);
}

TEST_F(MountAttributesTest, DetachedUnsupportedBackendRejectsRealIdmap) {
    Fd ns(user_namespace_fd()); ASSERT_GE(ns.get(), 0) << strerror(errno);
    Fd tree(syscall(SYS_open_tree, AT_FDCWD, "/proc", kClone | O_CLOEXEC));
    ASSERT_GE(tree.get(), 0) << strerror(errno);
    MountAttr attr; attr.set = kIdmap; attr.userns_fd = ns.get();
    EXPECT_EQ(-1, setattr(tree.get(), "", kEmptyPath, &attr)); EXPECT_EQ(EINVAL, errno);
    // Rejection cannot poison an otherwise usable detached mount object.
    attr.set = kNoexec; attr.userns_fd = UINT64_MAX;
    ASSERT_EQ(0, setattr(tree.get(), "", kEmptyPath, &attr)) << strerror(errno);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
