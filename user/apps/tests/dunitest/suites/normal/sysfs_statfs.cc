#include <gtest/gtest.h>

#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <string>
#include <sys/mount.h>
#include <sys/statfs.h>
#include <unistd.h>

namespace {
constexpr long kSysfsMagic = 0x62656572;
constexpr long kProcMagic = 0x9fa0;
constexpr long kCgroup2Magic = 0x63677270;

void ExpectIdentity(const std::string& path, long magic) {
    struct statfs by_path {}, by_fd {};
    ASSERT_EQ(0, statfs(path.c_str(), &by_path)) << path << ": " << strerror(errno);
    int fd = open(path.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    ASSERT_GE(fd, 0) << path << ": " << strerror(errno);
    const int result = fstatfs(fd, &by_fd);
    const int error = errno;
    EXPECT_EQ(0, close(fd));
    ASSERT_EQ(0, result) << strerror(error);
    EXPECT_EQ(magic, by_path.f_type) << path;
    EXPECT_EQ(magic, by_fd.f_type) << path;
    EXPECT_EQ(by_path.f_bsize, by_fd.f_bsize);
    EXPECT_EQ(by_path.f_namelen, by_fd.f_namelen);
    EXPECT_EQ(0, memcmp(&by_path.f_fsid, &by_fd.f_fsid, sizeof(by_path.f_fsid)));
}

class MountPoint {
public:
    MountPoint() {
        char path[] = "/tmp/sysfs-statfs-XXXXXX";
        if (mkdtemp(path)) path_ = path;
    }
    ~MountPoint() {
        if (mounted_) umount(path_.c_str());
        if (!path_.empty()) rmdir(path_.c_str());
    }
    const std::string& path() const { return path_; }
    int Mount(const char* source, const char* type, unsigned long flags = 0) {
        const int result = mount(source, path_.c_str(), type, flags, nullptr);
        if (result == 0) mounted_ = true;
        return result;
    }
private:
    std::string path_;
    bool mounted_ = false;
};

TEST(SysfsStatfs, ExistingRootAndDescendantsReportSysfsByPathAndFd) {
    for (const char* path : {"/sys", "/sys/devices", "/sys/class", "/sys/bus"}) {
        SCOPED_TRACE(path);
        ASSERT_NO_FATAL_FAILURE(ExpectIdentity(path, kSysfsMagic));
    }
}

TEST(SysfsStatfs, FreshAndBindMountsPreserveSysfsIdentity) {
    MountPoint fresh;
    ASSERT_FALSE(fresh.path().empty());
    ASSERT_EQ(0, fresh.Mount("sysfs", "sysfs")) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectIdentity(fresh.path(), kSysfsMagic));
    ASSERT_NO_FATAL_FAILURE(ExpectIdentity(fresh.path() + "/devices", kSysfsMagic));
    MountPoint bind;
    ASSERT_FALSE(bind.path().empty());
    ASSERT_EQ(0, bind.Mount(fresh.path().c_str(), nullptr, MS_BIND)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectIdentity(bind.path(), kSysfsMagic));
    ASSERT_NO_FATAL_FAILURE(ExpectIdentity(bind.path() + "/devices", kSysfsMagic));
}

TEST(SysfsStatfs, OtherFilesystemOwnersKeepTheirOwnIdentity) {
    ASSERT_NO_FATAL_FAILURE(ExpectIdentity("/proc", kProcMagic));
    MountPoint cgroup;
    ASSERT_FALSE(cgroup.path().empty());
    ASSERT_EQ(0, cgroup.Mount("none", "cgroup2")) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectIdentity(cgroup.path(), kCgroup2Magic));
    ASSERT_NO_FATAL_FAILURE(ExpectIdentity("/sys", kSysfsMagic));
}
} // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
