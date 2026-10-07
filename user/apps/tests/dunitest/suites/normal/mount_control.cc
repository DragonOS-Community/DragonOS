#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <string>
#include <vector>

#ifndef SYS_fsopen
#define SYS_fsopen 430
#endif
#ifndef SYS_fsconfig
#define SYS_fsconfig 431
#endif
#ifndef SYS_fsmount
#define SYS_fsmount 432
#endif
#ifndef SYS_fspick
#define SYS_fspick 433
#endif
#ifndef SYS_move_mount
#define SYS_move_mount 429
#endif

namespace {

constexpr unsigned kPickCloexec = 1;
constexpr unsigned kPickNofollow = 2;
constexpr unsigned kPickNoAutomount = 4;
constexpr unsigned kPickEmpty = 8;
constexpr unsigned kSetFlag = 0;
constexpr unsigned kSetString = 1;
constexpr unsigned kCreate = 6;
constexpr unsigned kReconfigure = 7;

class Fd {
public:
    explicit Fd(int fd) : fd_(fd) {}
    ~Fd() { if (fd_ >= 0) close(fd_); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return fd_; }
private:
    int fd_;
};

int pick(int dirfd, const char* path, unsigned flags = 0) {
    return syscall(SYS_fspick, dirfd, path, flags);
}

int config(int fd, unsigned cmd, const char* key = nullptr, const char* value = nullptr) {
    return syscall(SYS_fsconfig, fd, cmd, key, value, 0);
}

// Parse the complete option token rather than matching substrings (e.g. ro).
bool mount_option(const std::string& path, const char* wanted) {
    FILE* file = fopen("/proc/self/mounts", "r");
    if (!file) {
        ADD_FAILURE() << "read mount options: " << strerror(errno);
        return false;
    }
    char* line = nullptr;
    size_t capacity = 0;
    bool found = false;
    bool matched_mount = false;
    while (getline(&line, &capacity, file) >= 0) {
        char* save = nullptr;
        if (!strtok_r(line, " ", &save)) continue;
        char* target = strtok_r(nullptr, " ", &save);
        if (!target || path != target) continue;
        matched_mount = true;
        strtok_r(nullptr, " ", &save);
        char* options = strtok_r(nullptr, " \n", &save);
        char* option_save = nullptr;
        for (char* item = options ? strtok_r(options, ",", &option_save) : nullptr;
             item; item = strtok_r(nullptr, ",", &option_save)) {
            if (strcmp(item, wanted) == 0) found = true;
        }
        break;
    }
    free(line);
    fclose(file);
    EXPECT_TRUE(matched_mount) << "mount options absent for " << path;
    return found;
}

class MountControlTest : public ::testing::Test {
protected:
    void SetUp() override {
        // A failed environment prerequisite is a failure, never a skipped pass.
        ASSERT_EQ(0, unshare(CLONE_NEWNS)) << "requires CAP_SYS_ADMIN: " << strerror(errno);
        ASSERT_EQ(0, mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr))
            << strerror(errno);
        struct stat st {};
        if (stat("/tmp", &st) != 0) {
            ASSERT_EQ(0, mkdir("/tmp", 0777)) << strerror(errno);
        }
        char name[] = "/tmp/mount_control_XXXXXX";
        char* root = mkdtemp(name);
        ASSERT_NE(nullptr, root) << strerror(errno);
        root_ = root;
        src_ = root_ + "/src";
        alias_ = root_ + "/alias";
        ASSERT_EQ(0, mkdir(src_.c_str(), 0755)) << strerror(errno);
        ASSERT_EQ(0, mkdir(alias_.c_str(), 0755)) << strerror(errno);
        ASSERT_EQ(0, mount("mount_control", src_.c_str(), "tmpfs", 0, "mode=0755"))
            << strerror(errno);
        mounts_.push_back(src_);
    }

    void TearDown() override {
        for (auto it = mounts_.rbegin(); it != mounts_.rend(); ++it) {
            if (umount2(it->c_str(), MNT_DETACH) != 0 && errno != EINVAL && errno != ENOENT)
                ADD_FAILURE() << "cleanup " << *it << ": " << strerror(errno);
        }
        if (!root_.empty()) {
            unlink((root_ + "/link").c_str());
            rmdir(alias_.c_str());
            rmdir(src_.c_str());
            rmdir(root_.c_str());
        }
    }

    void bind_alias(unsigned flags = 0) {
        ASSERT_EQ(0, mount(src_.c_str(), alias_.c_str(), nullptr, MS_BIND, nullptr))
            << strerror(errno);
        mounts_.push_back(alias_);
        if (flags) {
            ASSERT_EQ(0, mount(nullptr, alias_.c_str(), nullptr,
                               MS_REMOUNT | MS_BIND | flags, nullptr)) << strerror(errno);
        }
    }

    std::string root_, src_, alias_;
    std::vector<std::string> mounts_;
};

TEST_F(MountControlTest, PickFlagsRootSymlinkAndEmptyPath) {
    Fd plain(pick(AT_FDCWD, src_.c_str()));
    ASSERT_GE(plain.get(), 0) << strerror(errno);
    EXPECT_EQ(0, fcntl(plain.get(), F_GETFD) & FD_CLOEXEC);
    Fd cloexec(pick(AT_FDCWD, src_.c_str(), kPickCloexec | kPickNoAutomount));
    ASSERT_GE(cloexec.get(), 0) << strerror(errno);
    EXPECT_NE(0, fcntl(cloexec.get(), F_GETFD) & FD_CLOEXEC);
    EXPECT_EQ(-1, pick(AT_FDCWD, src_.c_str(), 0x80000000U));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(-1, pick(AT_FDCWD, root_.c_str()));
    EXPECT_EQ(EINVAL, errno);
    const std::string child = src_ + "/child";
    ASSERT_EQ(0, mkdir(child.c_str(), 0755));
    EXPECT_EQ(-1, pick(AT_FDCWD, child.c_str()));
    EXPECT_EQ(EINVAL, errno);
    const std::string link = root_ + "/link";
    ASSERT_EQ(0, symlink(src_.c_str(), link.c_str()));
    Fd followed(pick(AT_FDCWD, link.c_str()));
    ASSERT_GE(followed.get(), 0) << strerror(errno);
    EXPECT_EQ(-1, pick(AT_FDCWD, link.c_str(), kPickNofollow));
    EXPECT_EQ(EINVAL, errno);
    Fd path(open(src_.c_str(), O_PATH | O_DIRECTORY | O_CLOEXEC));
    ASSERT_GE(path.get(), 0);
    Fd empty(pick(path.get(), "", kPickEmpty | kPickCloexec));
    ASSERT_GE(empty.get(), 0) << strerror(errno);
    EXPECT_NE(0, fcntl(empty.get(), F_GETFD) & FD_CLOEXEC);
    EXPECT_EQ(-1, pick(path.get(), "", 0));
    EXPECT_EQ(ENOENT, errno);
    EXPECT_EQ(-1, pick(-1, "", kPickEmpty));
    EXPECT_EQ(EBADF, errno);
}

TEST_F(MountControlTest, DupSharesContextAndPickCannotFsmount) {
    Fd fs(pick(AT_FDCWD, src_.c_str(), kPickCloexec));
    ASSERT_GE(fs.get(), 0) << strerror(errno);
    Fd copy(dup(fs.get()));
    ASSERT_GE(copy.get(), 0);
    EXPECT_EQ(0, fcntl(copy.get(), F_GETFD) & FD_CLOEXEC);
    ASSERT_EQ(0, config(copy.get(), kSetFlag, "sync")) << strerror(errno);
    ASSERT_EQ(0, config(fs.get(), kReconfigure)) << strerror(errno);
    EXPECT_TRUE(mount_option(src_, "sync"));
    EXPECT_EQ(-1, syscall(SYS_fsmount, copy.get(), 0, 0));
    EXPECT_EQ(EBUSY, errno);
    // The duplicate also sees the post-reconfigure lazy-clean phase.
    ASSERT_EQ(0, config(copy.get(), kReconfigure)) << strerror(errno);
    EXPECT_FALSE(mount_option(src_, "sync"));
}

TEST_F(MountControlTest, MultipleRoundsKeepCumulativeMaskAndUnselectedFlags) {
    // Legacy mount sets an SB flag the new API cannot select for reconfigure.
    ASSERT_EQ(0, umount(src_.c_str()));
    ASSERT_EQ(0, mount("mount_control", src_.c_str(), "tmpfs", MS_DIRSYNC, nullptr))
        << strerror(errno);
    Fd fs(pick(AT_FDCWD, src_.c_str()));
    ASSERT_GE(fs.get(), 0);
    EXPECT_TRUE(mount_option(src_, "dirsync"));
    ASSERT_EQ(0, config(fs.get(), kSetFlag, "sync"));
    ASSERT_EQ(0, config(fs.get(), kReconfigure));
    EXPECT_TRUE(mount_option(src_, "sync"));
    EXPECT_TRUE(mount_option(src_, "dirsync"));
    // Linux vfs_clean_context clears sb_flags but retains sb_flags_mask.
    ASSERT_EQ(0, config(fs.get(), kReconfigure));
    EXPECT_FALSE(mount_option(src_, "sync"));
    EXPECT_TRUE(mount_option(src_, "dirsync"));
    ASSERT_EQ(0, config(fs.get(), kSetFlag, "sync"));
    ASSERT_EQ(0, config(fs.get(), kReconfigure));
    EXPECT_TRUE(mount_option(src_, "sync"));
    ASSERT_EQ(0, config(fs.get(), kSetFlag, "async"));
    ASSERT_EQ(0, config(fs.get(), kReconfigure));
    EXPECT_FALSE(mount_option(src_, "sync"));
}

TEST_F(MountControlTest, ForbiddenDirsyncMaskFailsWithoutMutationAndPoisonsContext) {
    Fd fs(pick(AT_FDCWD, src_.c_str()));
    ASSERT_GE(fs.get(), 0);
    ASSERT_EQ(0, config(fs.get(), kSetFlag, "sync"));
    ASSERT_EQ(0, config(fs.get(), kSetFlag, "dirsync"));
    EXPECT_EQ(-1, config(fs.get(), kReconfigure));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_FALSE(mount_option(src_, "sync"));
    EXPECT_FALSE(mount_option(src_, "dirsync"));
    EXPECT_EQ(-1, config(fs.get(), kSetFlag, "async"));
    EXPECT_EQ(EBUSY, errno);
    EXPECT_EQ(-1, config(fs.get(), kReconfigure));
    EXPECT_EQ(EBUSY, errno);
    EXPECT_EQ(-1, config(fs.get(), kCreate));
    EXPECT_EQ(EBUSY, errno);
    Fd fresh(pick(AT_FDCWD, src_.c_str()));
    ASSERT_GE(fresh.get(), 0);
    ASSERT_EQ(0, config(fresh.get(), kReconfigure));
}

TEST_F(MountControlTest, ParameterParseErrorDoesNotEnterFailedPhase) {
    Fd fs(pick(AT_FDCWD, src_.c_str()));
    ASSERT_GE(fs.get(), 0);
    EXPECT_EQ(-1, config(fs.get(), kSetString, "size", "not-a-number"));
    EXPECT_EQ(EINVAL, errno);
    ASSERT_EQ(0, config(fs.get(), kSetFlag, "sync"));
    ASSERT_EQ(0, config(fs.get(), kReconfigure));
    EXPECT_TRUE(mount_option(src_, "sync"));
}

TEST_F(MountControlTest, InvalidCreateSizeIsRejectedAtSetStringAndCanBeRetried) {
    Fd defaults(syscall(SYS_fsopen, "tmpfs", kPickCloexec));
    ASSERT_GE(defaults.get(), 0) << strerror(errno);
    EXPECT_EQ(-1, config(defaults.get(), kSetString, "size", "not-a-number"));
    EXPECT_EQ(EINVAL, errno);
    // The rejected option must not be queued until Create, nor turn the
    // context into Failed. Creating without resubmitting it uses defaults.
    ASSERT_EQ(0, config(defaults.get(), kCreate)) << strerror(errno);
    Fd default_tree(syscall(SYS_fsmount, defaults.get(), kPickCloexec, 0));
    ASSERT_GE(default_tree.get(), 0) << strerror(errno);

    Fd retry(syscall(SYS_fsopen, "tmpfs", kPickCloexec));
    ASSERT_GE(retry.get(), 0) << strerror(errno);
    EXPECT_EQ(-1, config(retry.get(), kSetString, "size", "not-a-number"));
    EXPECT_EQ(EINVAL, errno);
    ASSERT_EQ(0, config(retry.get(), kSetString, "size", "1048576")) << strerror(errno);
    ASSERT_EQ(0, config(retry.get(), kCreate)) << strerror(errno);
    Fd retry_tree(syscall(SYS_fsmount, retry.get(), kPickCloexec, 0));
    ASSERT_GE(retry_tree.get(), 0) << strerror(errno);
}

TEST_F(MountControlTest, PickRetainsSuperblockWithoutBlockingOrdinaryUmount) {
    Fd fs(pick(AT_FDCWD, src_.c_str()));
    ASSERT_GE(fs.get(), 0);
    ASSERT_EQ(0, umount(src_.c_str())) << "fspick must not pin mount busy: " << strerror(errno);
    ASSERT_EQ(0, config(fs.get(), kSetFlag, "sync"));
    ASSERT_EQ(0, config(fs.get(), kReconfigure)) << "selected SB must stay active: " << strerror(errno);
    ASSERT_EQ(0, config(fs.get(), kReconfigure)) << strerror(errno);
}

TEST_F(MountControlTest, SuperblockFlagsAreSharedButMountEntryFlagsStayIsolated) {
    bind_alias(MS_RDONLY | MS_NOSUID | MS_NOEXEC | MS_NODEV);
    ASSERT_FALSE(HasFatalFailure());
    Fd fs(pick(AT_FDCWD, src_.c_str()));
    ASSERT_GE(fs.get(), 0);
    ASSERT_EQ(0, config(fs.get(), kSetFlag, "sync"));
    ASSERT_EQ(0, config(fs.get(), kReconfigure));
    EXPECT_TRUE(mount_option(src_, "sync"));
    EXPECT_TRUE(mount_option(alias_, "sync"));
    EXPECT_TRUE(mount_option(src_, "rw"));
    EXPECT_TRUE(mount_option(alias_, "ro"));
    EXPECT_TRUE(mount_option(alias_, "nosuid"));
    EXPECT_TRUE(mount_option(alias_, "noexec"));
    EXPECT_TRUE(mount_option(alias_, "nodev"));
    EXPECT_FALSE(mount_option(src_, "nosuid"));
    EXPECT_FALSE(mount_option(src_, "noexec"));
    EXPECT_FALSE(mount_option(src_, "nodev"));
    ASSERT_EQ(0, config(fs.get(), kSetFlag, "ro"));
    ASSERT_EQ(0, config(fs.get(), kReconfigure));
    EXPECT_TRUE(mount_option(src_, "ro"));
    EXPECT_TRUE(mount_option(alias_, "ro"));
    ASSERT_EQ(0, config(fs.get(), kSetFlag, "rw"));
    ASSERT_EQ(0, config(fs.get(), kReconfigure));
    EXPECT_TRUE(mount_option(src_, "rw"));
    EXPECT_TRUE(mount_option(alias_, "ro"));
    EXPECT_TRUE(mount_option(alias_, "nosuid"));
    EXPECT_TRUE(mount_option(alias_, "noexec"));
}

TEST_F(MountControlTest, FsmountTransitionsOriginalFsfdToReconfiguration) {
    Fd fs(syscall(SYS_fsopen, "tmpfs", 1));
    ASSERT_GE(fs.get(), 0) << strerror(errno);
    ASSERT_EQ(0, config(fs.get(), kCreate));
    // Before consuming the mount, fsconfig cannot reconfigure this context.
    EXPECT_EQ(-1, config(fs.get(), kReconfigure));
    EXPECT_EQ(EBUSY, errno);
    Fd tree(syscall(SYS_fsmount, fs.get(), 1, 0));
    ASSERT_GE(tree.get(), 0) << strerror(errno);
    ASSERT_EQ(0, syscall(SYS_move_mount, tree.get(), "", AT_FDCWD, alias_.c_str(), 4))
        << strerror(errno);
    mounts_.push_back(alias_);
    ASSERT_EQ(0, config(fs.get(), kSetFlag, "sync"));
    ASSERT_EQ(0, config(fs.get(), kReconfigure));
    EXPECT_TRUE(mount_option(alias_, "sync"));
    EXPECT_EQ(-1, syscall(SYS_fsmount, fs.get(), 0, 0));
    EXPECT_EQ(EBUSY, errno);
    ASSERT_EQ(0, config(fs.get(), kReconfigure));
    EXPECT_FALSE(mount_option(alias_, "sync"));
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
