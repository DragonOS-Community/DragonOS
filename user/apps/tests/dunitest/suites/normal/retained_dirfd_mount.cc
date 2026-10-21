#include <gtest/gtest.h>

#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <string>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#include <vector>

namespace {

constexpr unsigned long long kNoXdev = 0x01;
constexpr unsigned long long kBeneath = 0x08;
constexpr unsigned long long kInRoot = 0x10;
constexpr unsigned long long kCached = 0x20;

class RetainedDirfdMount : public ::testing::Test {
protected:
    void SetUp() override {
        char name[] = "/tmp/dunitest_retained_dirfd_XXXXXX";
        char* directory = mkdtemp(name);
        ASSERT_NE(nullptr, directory) << strerror(errno);
        root_ = directory;
        ASSERT_TRUE(Mount(root_)) << strerror(errno);
        anchor_ = root_ + "/anchor";
        ASSERT_EQ(0, mkdir(anchor_.c_str(), 0700)) << strerror(errno);
        fd_ = Hold(anchor_);
        ASSERT_GE(fd_, 0) << strerror(errno);
        cwd_ = Hold(".");
        ASSERT_GE(cwd_, 0) << strerror(errno);
    }

    void TearDown() override {
        if (cwd_ >= 0) {
            EXPECT_EQ(0, fchdir(cwd_));
        }
        for (int fd : fds_) EXPECT_EQ(0, close(fd));
        // Covering mounts are always added after mounts below them. Removing
        // them in reverse order exposes each original mountpoint for cleanup.
        for (auto it = mounts_.rbegin(); it != mounts_.rend(); ++it) {
            EXPECT_EQ(0, umount(it->c_str())) << *it << ": " << strerror(errno);
        }
        if (!root_.empty()) {
            EXPECT_EQ(0, rmdir(root_.c_str())) << strerror(errno);
        }
    }

    bool Mount(const std::string& path) {
        if (mount("tmpfs", path.c_str(), "tmpfs", 0, nullptr) != 0) return false;
        mounts_.push_back(path);
        return true;
    }

    int Hold(const std::string& path) {
        const int fd = open(path.c_str(), O_PATH | O_DIRECTORY);
        if (fd >= 0) fds_.push_back(fd);
        return fd;
    }

    bool Write(const std::string& path, const std::string& contents) {
        const int fd = open(path.c_str(), O_CREAT | O_WRONLY | O_TRUNC, 0600);
        if (fd < 0) return false;
        const ssize_t count = write(fd, contents.data(), contents.size());
        const int closed = close(fd);
        return count == static_cast<ssize_t>(contents.size()) && closed == 0;
    }

    int OpenAt2(int dirfd, const std::string& path, unsigned long long resolve) {
        struct OpenHow {
            unsigned long long flags, mode, resolve;
        } how{O_RDONLY, 0, resolve};
        return static_cast<int>(syscall(437, dirfd, path.c_str(), &how, sizeof(how)));
    }

    void ExpectContents(int fd, const std::string& contents) {
        ASSERT_GE(fd, 0) << strerror(errno);
        char buffer[64] = {};
        const ssize_t count = read(fd, buffer, sizeof(buffer));
        const int error = errno;
        EXPECT_EQ(0, close(fd));
        ASSERT_EQ(static_cast<ssize_t>(contents.size()), count) << strerror(error);
        EXPECT_EQ(contents, std::string(buffer, static_cast<size_t>(count)));
    }

    void ExpectSame(int fd, int expected) {
        ASSERT_GE(fd, 0) << strerror(errno);
        struct stat actual{}, reference{};
        const int status = fstat(fd, &actual);
        EXPECT_EQ(0, close(fd));
        ASSERT_EQ(0, status) << strerror(errno);
        ASSERT_EQ(0, fstat(expected, &reference)) << strerror(errno);
        EXPECT_EQ(reference.st_dev, actual.st_dev);
        EXPECT_EQ(reference.st_ino, actual.st_ino);
    }

    void ExpectError(int fd, int error) {
        const int actual = errno;
        if (fd >= 0) {
            EXPECT_EQ(0, close(fd));
        }
        EXPECT_EQ(-1, fd);
        EXPECT_EQ(error, actual);
    }

    void Cover() {
        ASSERT_TRUE(Write(anchor_ + "/marker", "original"));
        ASSERT_TRUE(Mount(anchor_)) << strerror(errno);
        ASSERT_TRUE(Write(anchor_ + "/marker", "cover"));
    }

    std::string root_, anchor_;
    std::vector<std::string> mounts_;
    std::vector<int> fds_;
    int fd_ = -1, cwd_ = -1;
};

TEST_F(RetainedDirfdMount, OpenAtReadsOriginalDirectoryAfterCover) {
    Cover();
    ASSERT_FALSE(HasFatalFailure());
    ExpectContents(open((anchor_ + "/marker").c_str(), O_RDONLY), "cover");
    ExpectContents(openat(fd_, "marker", O_RDONLY), "original");
}

TEST_F(RetainedDirfdMount, DotAndRepeatedDotsKeepOriginalIdentity) {
    Cover();
    ASSERT_FALSE(HasFatalFailure());
    ExpectSame(openat(fd_, ".", O_PATH | O_DIRECTORY), fd_);
    ExpectSame(openat(fd_, "././//", O_PATH | O_DIRECTORY), fd_);
    ExpectContents(openat(fd_, "././marker", O_RDONLY), "original");
}

TEST_F(RetainedDirfdMount, CurrentDirectoryKeepsRetainedPath) {
    ASSERT_EQ(0, fchdir(fd_)) << strerror(errno);
    Cover();
    ASSERT_FALSE(HasFatalFailure());
    ExpectContents(open("marker", O_RDONLY), "original");
    ExpectContents(openat(AT_FDCWD, "./marker", O_RDONLY), "original");
    ExpectSame(open(".", O_PATH | O_DIRECTORY), fd_);
}

TEST_F(RetainedDirfdMount, CreateUsesOriginalParent) {
    Cover();
    ASSERT_FALSE(HasFatalFailure());
    const int created = openat(fd_, "created", O_CREAT | O_EXCL | O_WRONLY, 0600);
    ASSERT_GE(created, 0) << strerror(errno);
    EXPECT_EQ(3, write(created, "new", 3));
    EXPECT_EQ(0, close(created));
    ExpectContents(openat(fd_, "created", O_RDONLY), "new");
    ExpectError(open((anchor_ + "/created").c_str(), O_RDONLY), ENOENT);
}

TEST_F(RetainedDirfdMount, RelativeSymlinkKeepsOriginalParent) {
    ASSERT_EQ(0, symlink(".", (anchor_ + "/relative").c_str())) << strerror(errno);
    Cover();
    ASSERT_FALSE(HasFatalFailure());
    ExpectContents(openat(fd_, "relative/marker", O_RDONLY), "original");
    ExpectSame(openat(fd_, "relative", O_PATH | O_DIRECTORY), fd_);
}

TEST_F(RetainedDirfdMount, AbsoluteSymlinkSelectsProcessRoot) {
    ASSERT_TRUE(Write(root_ + "/visible", "absolute"));
    ASSERT_EQ(0, symlink((root_ + "/visible").c_str(),
                         (anchor_ + "/absolute").c_str())) << strerror(errno);
    Cover();
    ASSERT_FALSE(HasFatalFailure());
    ExpectContents(openat(fd_, "absolute", O_RDONLY), "absolute");
    ExpectError(OpenAt2(fd_, "absolute", kBeneath), EXDEV);
}

TEST_F(RetainedDirfdMount, ExistingChildMountsStillFollowTop) {
    const std::string child = anchor_ + "/child";
    ASSERT_EQ(0, mkdir(child.c_str(), 0700)) << strerror(errno);
    ASSERT_TRUE(Mount(child)) << strerror(errno);
    ASSERT_TRUE(Write(child + "/marker", "lower"));
    ASSERT_TRUE(Mount(child)) << strerror(errno);
    ASSERT_TRUE(Write(child + "/marker", "top"));
    Cover();
    ASSERT_FALSE(HasFatalFailure());
    ExpectContents(openat(fd_, "child/marker", O_RDONLY), "top");
    ExpectError(OpenAt2(fd_, "child/marker", kNoXdev), EXDEV);
}

TEST_F(RetainedDirfdMount, DotDotFollowsOvermountOnParentResult) {
    const std::string child = anchor_ + "/child";
    ASSERT_EQ(0, mkdir(child.c_str(), 0700)) << strerror(errno);
    const int childfd = Hold(child);
    ASSERT_GE(childfd, 0) << strerror(errno);
    Cover();
    ASSERT_FALSE(HasFatalFailure());
    ExpectContents(openat(childfd, "../marker", O_RDONLY), "cover");
    ExpectError(OpenAt2(childfd, "../marker", kNoXdev), EXDEV);
}

TEST_F(RetainedDirfdMount, NoXdevAllowsSameMountBehindCover) {
    Cover();
    ASSERT_FALSE(HasFatalFailure());
    ExpectContents(OpenAt2(fd_, "marker", kNoXdev), "original");
    ExpectSame(OpenAt2(fd_, ".", kNoXdev), fd_);
}

TEST_F(RetainedDirfdMount, ScopedRootsKeepCoveredOriginalDirectory) {
    Cover();
    ASSERT_FALSE(HasFatalFailure());
    ExpectContents(OpenAt2(fd_, "marker", kBeneath), "original");
    ExpectContents(OpenAt2(fd_, "/marker", kInRoot), "original");
    ExpectSame(OpenAt2(fd_, "/", kInRoot), fd_);
    ExpectError(OpenAt2(fd_, "..", kBeneath), EXDEV);
    ExpectError(OpenAt2(fd_, "/marker", kBeneath), EXDEV);
}

TEST_F(RetainedDirfdMount, ScopedDotDotFollowsMountOnClampedRootResult) {
    Cover();
    ASSERT_FALSE(HasFatalFailure());
    // Linux clamps the parent to nd->root, then step_into still follows mounts
    // on that result. This differs from a root jump or an ordinary dot.
    ExpectContents(OpenAt2(fd_, "../../marker", kInRoot), "cover");
    const int coverfd = Hold(anchor_);
    ASSERT_GE(coverfd, 0) << strerror(errno);
    ExpectSame(OpenAt2(fd_, "..", kInRoot), coverfd);
    ExpectError(OpenAt2(fd_, "../marker", kInRoot | kNoXdev), EXDEV);
}

TEST_F(RetainedDirfdMount, BeneathChecksRootReachedDuringMountAscent) {
    ASSERT_EQ(0, mkdir((anchor_ + "/child").c_str(), 0700)) << strerror(errno);
    Cover();
    ASSERT_FALSE(HasFatalFailure());
    // The first parent step selects the covered scope root. The next parent
    // step must check the scope again while ascending that mount, before
    // selecting the parent of the underlying mountpoint.
    ExpectContents(OpenAt2(fd_, "child/../marker", kBeneath), "cover");
    ExpectError(OpenAt2(fd_, "child/../../marker", kBeneath), EXDEV);
    ExpectContents(OpenAt2(fd_, "child/../../marker", kInRoot), "cover");
}

TEST_F(RetainedDirfdMount, InRootClampsDuringStackedMountAscent) {
    ASSERT_EQ(0, mkdir((anchor_ + "/child").c_str(), 0700)) << strerror(errno);
    Cover();
    ASSERT_FALSE(HasFatalFailure());
    ASSERT_TRUE(Mount(anchor_)) << strerror(errno);
    ASSERT_TRUE(Write(anchor_ + "/marker", "stacked"));
    const int topfd = Hold(anchor_);
    ASSERT_GE(topfd, 0) << strerror(errno);
    ExpectContents(OpenAt2(fd_, "../../../marker", kInRoot), "stacked");
    ExpectContents(OpenAt2(fd_, "child/../../../marker", kInRoot), "stacked");
    ExpectSame(OpenAt2(fd_, "../../..", kInRoot), topfd);
    ExpectSame(OpenAt2(fd_, "/", kInRoot), fd_);
    ExpectError(OpenAt2(fd_, "child/../../marker", kBeneath), EXDEV);
}

TEST_F(RetainedDirfdMount, InRootNoXdevClampsAtMountRootBeforeCrossingMount) {
    const int rootfd = Hold(root_);
    ASSERT_GE(rootfd, 0) << strerror(errno);
    // A retained mount root is already the scope root. Clamping '..' is not
    // a mount crossing, even though the mount has a parent mountpoint.
    ExpectSame(OpenAt2(rootfd, "..", kInRoot | kNoXdev), rootfd);
    ASSERT_TRUE(Mount(root_)) << strerror(errno);
    // Once the clamped result has an overmount, following it would cross a
    // mount and NO_XDEV must reject that distinct operation.
    ExpectError(OpenAt2(rootfd, "..", kInRoot | kNoXdev), EXDEV);
}

TEST_F(RetainedDirfdMount, ProcessRootClampsCoveredMountWithoutNoXdevCrossing) {
    const int rootfd = Hold(root_);
    ASSERT_GE(rootfd, 0) << strerror(errno);
    ASSERT_TRUE(Mount(root_)) << strerror(errno);
    const int coverfd = Hold(root_);
    ASSERT_GE(coverfd, 0) << strerror(errno);
    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        // Isolate chroot from the fixture. The dirfd starts on the cover, while
        // the process root remains the lower mountpoint. Linux's upward root
        // check stops before a mount crossing and preserves the cover cursor.
        if (fchdir(rootfd) != 0 || chroot(".") != 0 || chdir("/") != 0) _exit(1);
        const int opened = OpenAt2(coverfd, "..", kNoXdev);
        if (opened < 0) _exit(2);
        struct stat actual{}, expected{};
        const bool same = fstat(opened, &actual) == 0 && fstat(coverfd, &expected) == 0 &&
                          actual.st_dev == expected.st_dev && actual.st_ino == expected.st_ino;
        const int closed = close(opened);
        _exit(same && closed == 0 ? 0 : 3);
    }
    int status = 0;
    pid_t waited;
    do {
        waited = waitpid(child, &status, 0);
    } while (waited < 0 && errno == EINTR);
    ASSERT_EQ(child, waited) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status)) << status;
    // 1: chroot setup, 2: openat2 error, 3: wrong identity or close error.
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST_F(RetainedDirfdMount, ScopedAbsoluteSymlinkUsesOriginalScopeRoot) {
    ASSERT_EQ(0, symlink("/marker", (anchor_ + "/absolute").c_str())) << strerror(errno);
    Cover();
    ASSERT_FALSE(HasFatalFailure());
    ExpectContents(OpenAt2(fd_, "absolute", kInRoot), "original");
}

TEST_F(RetainedDirfdMount, CachedInRootDotDotClampsWithoutTopology) {
    for (const char* path : {"..", "../..", "../../.."}) {
        ExpectSame(OpenAt2(fd_, path, kInRoot | kCached), fd_);
        ExpectSame(OpenAt2(fd_, path, kInRoot | kCached | kNoXdev), fd_);
    }
}

TEST_F(RetainedDirfdMount, CachedBeneathDotDotRequiresRefWalk) {
    ExpectError(OpenAt2(fd_, "..", kBeneath | kCached), EAGAIN);
    ExpectError(OpenAt2(fd_, "..", kBeneath | kCached | kNoXdev), EAGAIN);
    ExpectError(OpenAt2(fd_, "..", kBeneath), EXDEV);
    ExpectSame(OpenAt2(fd_, ".", kBeneath | kCached), fd_);
}

TEST_F(RetainedDirfdMount, CachedProcessRootDotDotClamps) {
    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (fchdir(fd_) != 0 || chroot(".") != 0 || chdir("/") != 0) _exit(1);
        for (auto resolve : {kCached, kCached | kNoXdev}) {
            const int opened = OpenAt2(AT_FDCWD, "/../../..", resolve);
            if (opened < 0) _exit(2);
            struct stat actual{}, expected{};
            const bool same = fstat(opened, &actual) == 0 && fstat(fd_, &expected) == 0 &&
                              actual.st_dev == expected.st_dev && actual.st_ino == expected.st_ino;
            if (close(opened) != 0 || !same) _exit(3);
        }
        _exit(0);
    }
    int status = 0;
    pid_t waited;
    do {
        waited = waitpid(child, &status, 0);
    } while (waited < 0 && errno == EINTR);
    ASSERT_EQ(child, waited) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status)) << status;
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST_F(RetainedDirfdMount, CachedClampedRootStillChecksCoverMount) {
    Cover();
    ASSERT_FALSE(HasFatalFailure());
    const int coverfd = Hold(anchor_);
    ASSERT_GE(coverfd, 0) << strerror(errno);
    // Linux may follow an already cached cover. A try-only implementation may
    // return EAGAIN; success must still select the cover, never the lower root.
    const int cached_parent = OpenAt2(fd_, "..", kInRoot | kCached);
    if (cached_parent >= 0) ExpectSame(cached_parent, coverfd);
    else EXPECT_EQ(EAGAIN, errno);
    const int cached_file = OpenAt2(fd_, "../marker", kInRoot | kCached);
    if (cached_file >= 0) ExpectContents(cached_file, "cover");
    else EXPECT_EQ(EAGAIN, errno);
    ExpectError(OpenAt2(fd_, "..", kInRoot | kCached | kNoXdev), EAGAIN);
    ExpectSame(OpenAt2(fd_, "..", kInRoot), coverfd);
    ExpectError(OpenAt2(fd_, "..", kInRoot | kNoXdev), EXDEV);
    ExpectSame(OpenAt2(fd_, ".", kInRoot | kCached), fd_);
}

TEST_F(RetainedDirfdMount, CachedWarmLookupDoesNotReenterCover) {
    ASSERT_TRUE(Write(anchor_ + "/marker", "original"));
    ExpectContents(openat(fd_, "marker", O_RDONLY), "original");
    ASSERT_TRUE(Mount(anchor_)) << strerror(errno);
    ASSERT_TRUE(Write(anchor_ + "/marker", "cover"));
    // The original tmpfs dentry is positive and warm. EAGAIN must not hide a
    // blanket cache-only refusal or selection of the covered directory.
    ExpectContents(OpenAt2(fd_, "marker", kCached), "original");
    ExpectSame(OpenAt2(fd_, ".", kCached), fd_);
    ExpectContents(OpenAt2(fd_, "marker", kCached | kNoXdev), "original");
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
