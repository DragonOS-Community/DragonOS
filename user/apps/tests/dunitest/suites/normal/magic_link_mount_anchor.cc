#include <gtest/gtest.h>

#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <string>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <vector>

namespace {

class MagicLinkMountAnchor : public ::testing::Test {
protected:
    void SetUp() override {
        char name[] = "/tmp/dunitest_magic_anchor_XXXXXX";
        char* directory = mkdtemp(name);
        ASSERT_NE(nullptr, directory) << strerror(errno);
        directory_ = directory;
        fd_ = open(directory_.c_str(), O_PATH | O_DIRECTORY);
        ASSERT_GE(fd_, 0) << strerror(errno);
        link_ = "/proc/self/fd/" + std::to_string(fd_);
    }

    void TearDown() override {
        for (int fd : extra_fds_) close(fd);
        if (fd_ >= 0) close(fd_);
        for (auto it = mounts_.rbegin(); it != mounts_.rend(); ++it) {
            EXPECT_EQ(0, umount(it->c_str())) << *it << ": " << strerror(errno);
        }
        for (const auto& file : files_) unlink(file.c_str());
        for (auto it = directories_.rbegin(); it != directories_.rend(); ++it) rmdir(it->c_str());
        if (!directory_.empty()) rmdir(directory_.c_str());
    }

    bool MakeDirectory(const std::string& path) {
        if (mkdir(path.c_str(), 0700) != 0) return false;
        directories_.push_back(path);
        return true;
    }

    bool WriteFile(const std::string& path, const std::string& text) {
        int fd = open(path.c_str(), O_CREAT | O_WRONLY | O_TRUNC, 0600);
        if (fd < 0) return false;
        files_.push_back(path);
        const ssize_t written = write(fd, text.data(), text.size());
        const int closed = close(fd);
        return written == static_cast<ssize_t>(text.size()) && closed == 0;
    }

    bool MakeSymlink(const std::string& target, const std::string& path) {
        if (symlink(target.c_str(), path.c_str()) != 0) return false;
        files_.push_back(path);
        return true;
    }

    bool MountTmpfs(const std::string& path) {
        if (mount("tmpfs", path.c_str(), "tmpfs", 0, nullptr) != 0) return false;
        mounts_.push_back(path);
        return true;
    }

    std::string HoldDirectory(const std::string& path) {
        int fd = open(path.c_str(), O_PATH | O_DIRECTORY);
        if (fd < 0) return {};
        extra_fds_.push_back(fd);
        return "/proc/self/fd/" + std::to_string(fd);
    }

    void ExpectContents(const std::string& path, const std::string& text) {
        int fd = open(path.c_str(), O_RDONLY);
        ASSERT_GE(fd, 0) << path << ": " << strerror(errno);
        char buffer[64] = {};
        const ssize_t count = read(fd, buffer, sizeof(buffer));
        EXPECT_EQ(0, close(fd));
        ASSERT_EQ(static_cast<ssize_t>(text.size()), count) << path;
        EXPECT_EQ(text, std::string(buffer, static_cast<size_t>(count)));
    }

    void ExpectSame(const std::string& actual, const std::string& expected) {
        struct stat observed{}, reference{};
        ASSERT_EQ(0, stat(expected.c_str(), &reference)) << expected << ": " << strerror(errno);
        ASSERT_EQ(0, stat(actual.c_str(), &observed)) << actual << ": " << strerror(errno);
        EXPECT_EQ(reference.st_dev, observed.st_dev);
        EXPECT_EQ(reference.st_ino, observed.st_ino);
    }

    std::string directory_, link_;
    std::vector<std::string> mounts_, files_, directories_;
    std::vector<int> extra_fds_;
    int fd_ = -1;
};

TEST_F(MagicLinkMountAnchor, DotPreservesDirectory) {
    ExpectSame(link_ + "/.", directory_);
}

TEST_F(MagicLinkMountAnchor, DotDotUsesTargetParent) {
    ExpectSame(link_ + "/..", directory_ + "/..");
}

TEST_F(MagicLinkMountAnchor, RepeatedDotsPreserveDirectory) {
    ExpectSame(link_ + "/././.", directory_);
}

TEST_F(MagicLinkMountAnchor, FirstChildFollowsExistingMount) {
    int root = open("/", O_PATH | O_DIRECTORY);
    ASSERT_GE(root, 0) << strerror(errno);
    const std::string child = "/proc/self/fd/" + std::to_string(root) + "/proc";
    ExpectSame(child, "/proc");
    EXPECT_EQ(0, close(root));
}

TEST_F(MagicLinkMountAnchor, CoveredTargetKeepsOriginalDirectory) {
    ASSERT_TRUE(WriteFile(directory_ + "/marker", "original"));
    ASSERT_TRUE(MountTmpfs(directory_)) << strerror(errno);
    ASSERT_TRUE(WriteFile(directory_ + "/marker", "cover"));
    ExpectContents(directory_ + "/marker", "cover");
    ExpectContents(link_ + "/marker", "original");
    ExpectContents(link_ + "/././marker", "original");
    ExpectSame(link_ + "/.///", link_);
}

TEST_F(MagicLinkMountAnchor, RelativeSymlinkRetainsOriginalCursor) {
    ASSERT_TRUE(WriteFile(directory_ + "/marker", "original"));
    ASSERT_TRUE(MakeSymlink(".", directory_ + "/relative"));
    ASSERT_TRUE(MountTmpfs(directory_)) << strerror(errno);
    ASSERT_TRUE(WriteFile(directory_ + "/marker", "cover"));
    ExpectContents(link_ + "/relative/marker", "original");
    ExpectSame(link_ + "/relative", link_);
}

TEST_F(MagicLinkMountAnchor, AbsoluteSymlinkSelectsProcessRoot) {
    ASSERT_TRUE(MakeSymlink("/proc", directory_ + "/absolute"));
    ASSERT_TRUE(MountTmpfs(directory_)) << strerror(errno);
    ExpectSame(link_ + "/absolute", "/proc");
    ExpectSame(link_ + "/absolute/self", "/proc/self");
}

TEST_F(MagicLinkMountAnchor, FirstChildFollowsStackedMountsBelowCoveredTarget) {
    const std::string child = directory_ + "/child";
    ASSERT_TRUE(MakeDirectory(child));
    ASSERT_TRUE(MountTmpfs(child)) << strerror(errno);
    ASSERT_TRUE(WriteFile(child + "/marker", "lower mount"));
    ASSERT_TRUE(MountTmpfs(child)) << strerror(errno);
    ASSERT_TRUE(WriteFile(child + "/marker", "top mount"));
    const std::string retained_child = HoldDirectory(child);
    ASSERT_FALSE(retained_child.empty());
    ASSERT_TRUE(MountTmpfs(directory_)) << strerror(errno);
    ExpectSame(link_ + "/child", retained_child);
    ExpectSame(link_ + "/./child/.", retained_child);
    ExpectContents(link_ + "/child/marker", "top mount");
}

TEST_F(MagicLinkMountAnchor, DotDotFollowsMountOnParentResult) {
    const std::string child = directory_ + "/child";
    ASSERT_TRUE(MakeDirectory(child));
    const std::string retained_child = HoldDirectory(child);
    ASSERT_FALSE(retained_child.empty());
    ASSERT_TRUE(MountTmpfs(directory_)) << strerror(errno);
    ASSERT_TRUE(WriteFile(directory_ + "/marker", "parent cover"));
    ExpectSame(retained_child + "/..", directory_);
    ExpectContents(retained_child + "/../marker", "parent cover");
}

TEST_F(MagicLinkMountAnchor, DotDotLeavesMountRoot) {
    const std::string child = directory_ + "/child";
    ASSERT_TRUE(MakeDirectory(child));
    ASSERT_TRUE(MountTmpfs(child)) << strerror(errno);
    const std::string retained_child = HoldDirectory(child);
    ASSERT_FALSE(retained_child.empty());
    ExpectSame(retained_child + "/..", directory_);
}

TEST_F(MagicLinkMountAnchor, OpenAt2RestrictionsRemainEffective) {
    struct OpenHow { unsigned long long flags, mode, resolve; };
    const struct { unsigned long long resolve; int error; } cases[] = {
        {0x01, EXDEV}, {0x02, ELOOP}, {0x04, ELOOP}, {0x20, EAGAIN},
    };
    for (const auto& test : cases) {
        OpenHow how{O_PATH | O_DIRECTORY, 0, test.resolve};
        errno = 0;
        const int fd = static_cast<int>(syscall(437, AT_FDCWD, (link_ + "/.").c_str(),
                                                &how, sizeof(how)));
        EXPECT_EQ(-1, fd) << "resolve=" << test.resolve;
        const int error = errno;
        if (fd >= 0) close(fd);
        EXPECT_EQ(test.error, error) << "resolve=" << test.resolve;
    }
    // Start inside procfs so NO_XDEV fails at the magic jump itself, rather
    // than at the earlier transition from / into the procfs mount.
    const int procfd = open("/proc/self/fd", O_PATH | O_DIRECTORY);
    ASSERT_GE(procfd, 0) << strerror(errno);
    OpenHow how{O_PATH | O_DIRECTORY, 0, 0x01};
    const std::string relative = std::to_string(fd_) + "/.";
    errno = 0;
    const int result = static_cast<int>(syscall(437, procfd, relative.c_str(), &how, sizeof(how)));
    const int error = errno;
    if (result >= 0) close(result);
    EXPECT_EQ(0, close(procfd));
    EXPECT_EQ(-1, result);
    EXPECT_EQ(EXDEV, error);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
