#include <gtest/gtest.h>

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <unistd.h>

#include <string>

namespace {

constexpr char kPath[] = "/proc/sys/kernel/cap_last_cap";

TEST(ProcCapLastCap, ExposesKernelCapabilityBoundary) {
    struct stat st = {};
    ASSERT_EQ(0, stat(kPath, &st));
    EXPECT_TRUE(S_ISREG(st.st_mode));
    EXPECT_EQ(0444u, static_cast<unsigned>(st.st_mode & 0777));

    int fd = open(kPath, O_RDONLY);
    ASSERT_GE(fd, 0);
    char buf[32] = {};
    ssize_t n = read(fd, buf, sizeof(buf));
    ASSERT_GT(n, 0);
    EXPECT_EQ(std::to_string(CAP_LAST_CAP) + "\n", std::string(buf, n));
    EXPECT_EQ(0, read(fd, buf, sizeof(buf)));
    close(fd);

    errno = 0;
    EXPECT_GE(prctl(PR_CAPBSET_READ, CAP_LAST_CAP, 0, 0, 0), 0);
    EXPECT_EQ(-1, prctl(PR_CAPBSET_READ, CAP_LAST_CAP + 1, 0, 0, 0));
    EXPECT_EQ(EINVAL, errno);
}

TEST(ProcCapLastCap, NumericSysctlShortReadEndsRecord) {
    int fd = open(kPath, O_RDONLY);
    ASSERT_GE(fd, 0);
    char c = 0;
    ASSERT_EQ(1, read(fd, &c, 1));
    EXPECT_EQ('4', c);
    EXPECT_EQ(0, read(fd, &c, 1));

    ASSERT_EQ(0, lseek(fd, 0, SEEK_SET));
    char buf[32] = {};
    ssize_t n = read(fd, buf, sizeof(buf));
    ASSERT_GT(n, 0);
    EXPECT_EQ(std::to_string(CAP_LAST_CAP) + "\n", std::string(buf, n));
    close(fd);
}

TEST(ProcCapLastCap, AppearsInKernelSysctlDirectory) {
    DIR* dir = opendir("/proc/sys/kernel");
    ASSERT_NE(nullptr, dir);
    bool found = false;
    while (dirent* entry = readdir(dir)) {
        if (std::string(entry->d_name) == "cap_last_cap") {
            found = true;
            break;
        }
    }
    closedir(dir);
    EXPECT_TRUE(found);
}

TEST(ProcCapLastCap, CannotBeMadeWritable) {
    errno = 0;
    EXPECT_EQ(-1, access(kPath, W_OK));
    EXPECT_EQ(EACCES, errno);

    errno = 0;
    EXPECT_EQ(-1, open(kPath, O_WRONLY));
    EXPECT_EQ(EACCES, errno);
    errno = 0;
    EXPECT_EQ(-1, open(kPath, O_RDWR));
    EXPECT_EQ(EACCES, errno);

    errno = 0;
    EXPECT_EQ(-1, chmod(kPath, 0666));
    EXPECT_EQ(EPERM, errno);
    errno = 0;
    EXPECT_EQ(-1, chown(kPath, 0, 0));
    EXPECT_EQ(EPERM, errno);

    struct stat st = {};
    ASSERT_EQ(0, stat(kPath, &st));
    EXPECT_EQ(0444u, static_cast<unsigned>(st.st_mode & 0777));
    EXPECT_EQ(0u, st.st_uid);
    EXPECT_EQ(0u, st.st_gid);
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
