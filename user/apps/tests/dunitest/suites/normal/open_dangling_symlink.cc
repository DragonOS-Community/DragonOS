#include <gtest/gtest.h>

#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <string>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

class OpenDanglingSymlinkTest : public ::testing::Test {
protected:
    void SetUp() override {
        char path[] = "/tmp/dunitest_open_symlink_XXXXXX";
        char* created = mkdtemp(path);
        ASSERT_NE(nullptr, created) << std::strerror(errno);
        dir_ = created;
    }

    void TearDown() override {
        unlink(Path("link").c_str());
        unlink(Path("target").c_str());
        unlink(Path("sub/l2").c_str());
        rmdir(Path("sub").c_str());
        rmdir(dir_.c_str());
    }

    std::string Path(const char* name) const { return dir_ + "/" + name; }

    void ExpectRegularFile(const std::string& path) {
        struct stat st = {};
        ASSERT_EQ(0, stat(path.c_str(), &st)) << std::strerror(errno);
        EXPECT_TRUE(S_ISREG(st.st_mode));
    }

    std::string dir_;
};

TEST_F(OpenDanglingSymlinkTest, CreatesRelativeTargetAfterFollowingFinalSymlink) {
    ASSERT_EQ(0, symlink("target", Path("link").c_str())) << std::strerror(errno);

    int fd = open(Path("link").c_str(), O_CREAT | O_RDWR, 0600);
    ASSERT_GE(fd, 0) << std::strerror(errno);
    EXPECT_EQ(0, close(fd));

    struct stat link_st = {};
    ASSERT_EQ(0, lstat(Path("link").c_str(), &link_st));
    EXPECT_TRUE(S_ISLNK(link_st.st_mode));
    ExpectRegularFile(Path("target"));
}

TEST_F(OpenDanglingSymlinkTest, EmptyTargetIsRejectedByBothSyscalls) {
    errno = 0;
    EXPECT_EQ(-1, symlink("", Path("link").c_str()));
    EXPECT_EQ(ENOENT, errno);

    int dirfd = open(dir_.c_str(), O_RDONLY | O_DIRECTORY);
    ASSERT_GE(dirfd, 0) << std::strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, symlinkat("", dirfd, "link"));
    EXPECT_EQ(ENOENT, errno);
    EXPECT_EQ(0, close(dirfd));

    struct stat st = {};
    errno = 0;
    EXPECT_EQ(-1, lstat(Path("link").c_str(), &st));
    EXPECT_EQ(ENOENT, errno);
}

TEST_F(OpenDanglingSymlinkTest, TrailingSlashDoesNotCreateFinalComponent) {
    errno = 0;
    EXPECT_EQ(-1, symlink("target", (Path("link") + "/").c_str()));
    EXPECT_EQ(ENOENT, errno);

    struct stat st = {};
    errno = 0;
    EXPECT_EQ(-1, lstat(Path("link").c_str(), &st));
    EXPECT_EQ(ENOENT, errno);
}

TEST_F(OpenDanglingSymlinkTest, CreationRequiresParentWritePermission) {
    ASSERT_EQ(0, chmod(dir_.c_str(), 0555)) << std::strerror(errno);
    pid_t child = fork();
    ASSERT_GE(child, 0) << std::strerror(errno);
    if (child == 0) {
        if (setgid(1000) != 0 || setuid(1000) != 0) {
            _exit(2);
        }
        errno = 0;
        int result = symlink("target", Path("link").c_str());
        _exit(result == -1 && errno == EACCES ? 0 : 1);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << std::strerror(errno);
    EXPECT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    ASSERT_EQ(0, chmod(dir_.c_str(), 0700)) << std::strerror(errno);

    struct stat st = {};
    errno = 0;
    EXPECT_EQ(-1, lstat(Path("link").c_str(), &st));
    EXPECT_EQ(ENOENT, errno);
}

TEST_F(OpenDanglingSymlinkTest, CreatesAbsoluteTarget) {
    const std::string target = Path("target");
    ASSERT_EQ(0, symlink(target.c_str(), Path("link").c_str())) << std::strerror(errno);

    int fd = open(Path("link").c_str(), O_CREAT | O_RDWR, 0600);
    ASSERT_GE(fd, 0) << std::strerror(errno);
    EXPECT_EQ(0, close(fd));
    ExpectRegularFile(target);
}

TEST_F(OpenDanglingSymlinkTest, CreatesTargetAfterNestedRelativeSymlinks) {
    ASSERT_EQ(0, mkdir(Path("sub").c_str(), 0700)) << std::strerror(errno);
    ASSERT_EQ(0, symlink("../target", Path("sub/l2").c_str())) << std::strerror(errno);
    ASSERT_EQ(0, symlink("sub/l2", Path("link").c_str())) << std::strerror(errno);

    int fd = open(Path("link").c_str(), O_CREAT | O_RDWR, 0600);
    ASSERT_GE(fd, 0) << std::strerror(errno);
    EXPECT_EQ(0, close(fd));
    ExpectRegularFile(Path("target"));
}

TEST_F(OpenDanglingSymlinkTest, ExclusiveCreateDoesNotFollowFinalSymlink) {
    ASSERT_EQ(0, symlink("target", Path("link").c_str())) << std::strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, open(Path("link").c_str(), O_CREAT | O_EXCL | O_RDWR, 0600));
    EXPECT_EQ(EEXIST, errno);
    EXPECT_EQ(-1, access(Path("target").c_str(), F_OK));
    EXPECT_EQ(ENOENT, errno);
}

TEST_F(OpenDanglingSymlinkTest, NoFollowRejectsFinalSymlink) {
    ASSERT_EQ(0, symlink("target", Path("link").c_str())) << std::strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, open(Path("link").c_str(), O_CREAT | O_NOFOLLOW | O_RDWR, 0600));
    EXPECT_EQ(ELOOP, errno);
    EXPECT_EQ(-1, access(Path("target").c_str(), F_OK));
    EXPECT_EQ(ENOENT, errno);
}

TEST_F(OpenDanglingSymlinkTest, TrailingSlashInTargetPreventsRegularCreate) {
    ASSERT_EQ(0, symlink("target/", Path("link").c_str())) << std::strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, open(Path("link").c_str(), O_CREAT | O_RDWR, 0600));
    EXPECT_EQ(EISDIR, errno);
    EXPECT_EQ(-1, access(Path("target").c_str(), F_OK));
    EXPECT_EQ(ENOENT, errno);
}

TEST_F(OpenDanglingSymlinkTest, TrailingSlashInOriginalPathPreventsRegularCreate) {
    ASSERT_EQ(0, symlink("target", Path("link").c_str())) << std::strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, open((Path("link") + "/").c_str(), O_CREAT | O_RDWR, 0600));
    EXPECT_EQ(EISDIR, errno);
    EXPECT_EQ(-1, access(Path("target").c_str(), F_OK));
    EXPECT_EQ(ENOENT, errno);
}

TEST_F(OpenDanglingSymlinkTest, MissingIntermediateDirectoryIsNotCreated) {
    ASSERT_EQ(0, symlink("missing/target", Path("link").c_str())) << std::strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, open(Path("link").c_str(), O_CREAT | O_RDWR, 0600));
    EXPECT_EQ(ENOENT, errno);
    EXPECT_EQ(-1, access(Path("missing").c_str(), F_OK));
    EXPECT_EQ(ENOENT, errno);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
