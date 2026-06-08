#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/sysmacros.h>
#include <unistd.h>

#include <fstream>
#include <sstream>
#include <string>

namespace {

constexpr long kTmpfsMagic = 0x01021994;
constexpr const char* kMountPoint = "/tmp/dunitest-devtmpfs-mnt";
constexpr const char* kTmpfsMountPoint = "/tmp/dunitest-tmpfs-special-mnt";

std::string ReadLink(const char* path) {
    char buf[256];
    ssize_t len = readlink(path, buf, sizeof(buf) - 1);
    if (len < 0) {
        return {};
    }
    buf[len] = '\0';
    return std::string(buf, static_cast<size_t>(len));
}

bool ProcMountsHasDevtmpfsAt(const std::string& path) {
    std::ifstream in("/proc/self/mounts");
    std::string source;
    std::string target;
    std::string fstype;
    while (in >> source >> target >> fstype) {
        std::string rest;
        std::getline(in, rest);
        if (target == path && fstype == "devtmpfs") {
            return true;
        }
    }
    return false;
}

bool MountInfoHasDevtmpfsAt(const std::string& path) {
    std::ifstream in("/proc/self/mountinfo");
    std::string line;
    while (std::getline(in, line)) {
        std::istringstream iss(line);
        std::string field;
        for (int i = 0; i < 4; ++i) {
            if (!(iss >> field)) {
                return false;
            }
        }

        std::string mount_point;
        if (!(iss >> mount_point)) {
            return false;
        }
        while (iss >> field && field != "-") {
        }
        std::string fstype;
        if (iss >> fstype) {
            if (mount_point == path && fstype == "devtmpfs") {
                return true;
            }
        }
    }
    return false;
}

void ExpectCharDeviceNumber(const char* path, unsigned int major_num, unsigned int minor_num) {
    struct stat st = {};
    ASSERT_EQ(0, stat(path, &st)) << "stat(" << path << ") failed: errno=" << errno << " ("
                                  << strerror(errno) << ")";
    EXPECT_TRUE(S_ISCHR(st.st_mode)) << path << " is not a character device";
    EXPECT_EQ(major_num, major(st.st_rdev)) << path << " major mismatch";
    EXPECT_EQ(minor_num, minor(st.st_rdev)) << path << " minor mismatch";
    EXPECT_EQ(0u, st.st_uid) << path << " uid mismatch";
    EXPECT_EQ(0u, st.st_gid) << path << " gid mismatch";
}

void ExpectReadZeros(const char* path) {
    int fd = open(path, O_RDONLY);
    ASSERT_GE(fd, 0) << "open(" << path << ") failed: errno=" << errno << " ("
                     << strerror(errno) << ")";

    char buf[8];
    memset(buf, 0x7f, sizeof(buf));
    ASSERT_EQ(static_cast<ssize_t>(sizeof(buf)), read(fd, buf, sizeof(buf)))
        << "read(" << path << ") failed: errno=" << errno << " (" << strerror(errno) << ")";
    for (char c : buf) {
        EXPECT_EQ(0, c);
    }
    EXPECT_EQ(0, close(fd));
}

void EnsureMountPoint(const char* mount_point = kMountPoint) {
    mkdir("/tmp", 0777);
    umount(mount_point);
    mkdir(mount_point, 0755);
}

}  // namespace

TEST(DevtmpfsSemantics, DevMountExportsLinuxTypeAndStatfs) {
    struct statfs st = {};
    ASSERT_EQ(0, statfs("/dev", &st)) << "statfs(/dev) failed: errno=" << errno << " ("
                                      << strerror(errno) << ")";
    EXPECT_EQ(kTmpfsMagic, st.f_type);
    EXPECT_EQ(255, st.f_namelen);
    EXPECT_GT(st.f_bsize, 0);

    EXPECT_TRUE(ProcMountsHasDevtmpfsAt("/dev"));
    EXPECT_TRUE(MountInfoHasDevtmpfsAt("/dev"));
}

TEST(DevtmpfsSemantics, BuiltinDeviceNumbersAndLinks) {
    ExpectCharDeviceNumber("/dev/null", 1, 3);
    ExpectCharDeviceNumber("/dev/zero", 1, 5);
    ExpectCharDeviceNumber("/dev/full", 1, 7);
    ExpectCharDeviceNumber("/dev/random", 1, 8);
    ExpectCharDeviceNumber("/dev/urandom", 1, 9);
    ExpectCharDeviceNumber("/dev/ptmx", 5, 2);

    EXPECT_EQ("/proc/self/fd", ReadLink("/dev/fd"));
    ExpectReadZeros("/dev/zero");
}

TEST(DevtmpfsSemantics, PublicMountReusesKernelInstance) {
    EnsureMountPoint();
    ASSERT_EQ(0, mount("devtmpfs", kMountPoint, "devtmpfs", 0, nullptr))
        << "mount(devtmpfs) failed: errno=" << errno << " (" << strerror(errno) << ")";

    struct statfs st = {};
    EXPECT_EQ(0, statfs(kMountPoint, &st)) << strerror(errno);
    EXPECT_EQ(kTmpfsMagic, st.f_type);
    EXPECT_TRUE(ProcMountsHasDevtmpfsAt(kMountPoint));
    EXPECT_TRUE(MountInfoHasDevtmpfsAt(kMountPoint));

    std::string mounted_null = std::string(kMountPoint) + "/null";
    std::string mounted_zero = std::string(kMountPoint) + "/zero";
    ExpectCharDeviceNumber(mounted_null.c_str(), 1, 3);
    ExpectCharDeviceNumber(mounted_zero.c_str(), 1, 5);
    ExpectReadZeros(mounted_zero.c_str());

    ASSERT_EQ(0, umount(kMountPoint)) << "umount(" << kMountPoint << ") failed: errno=" << errno
                                      << " (" << strerror(errno) << ")";
}

TEST(DevtmpfsSemantics, ManualMknodResolvesRegisteredDeviceNumber) {
    EnsureMountPoint();
    ASSERT_EQ(0, mount("devtmpfs", kMountPoint, "devtmpfs", 0, nullptr))
        << "mount(devtmpfs) failed: errno=" << errno << " (" << strerror(errno) << ")";

    std::string manual_zero = std::string(kMountPoint) + "/manual-zero";
    unlink(manual_zero.c_str());
    ASSERT_EQ(0, mknod(manual_zero.c_str(), S_IFCHR | 0600, makedev(1, 5)))
        << "mknod(" << manual_zero << ") failed: errno=" << errno << " (" << strerror(errno)
        << ")";
    ExpectCharDeviceNumber(manual_zero.c_str(), 1, 5);
    ExpectReadZeros(manual_zero.c_str());

    std::string block_zero = std::string(kMountPoint) + "/block-zero";
    unlink(block_zero.c_str());
    ASSERT_EQ(0, mknod(block_zero.c_str(), S_IFBLK | 0600, makedev(1, 5)))
        << "mknod(" << block_zero << ") failed: errno=" << errno << " (" << strerror(errno)
        << ")";
    errno = 0;
    int block_fd = open(block_zero.c_str(), O_RDONLY);
    EXPECT_EQ(-1, block_fd);
    EXPECT_EQ(ENXIO, errno);
    if (block_fd >= 0) {
        close(block_fd);
    }

    std::string missing = std::string(kMountPoint) + "/manual-missing";
    unlink(missing.c_str());
    ASSERT_EQ(0, mknod(missing.c_str(), S_IFCHR | 0600, makedev(250, 250)))
        << "mknod(" << missing << ") failed: errno=" << errno << " (" << strerror(errno) << ")";

    errno = 0;
    int fd = open(missing.c_str(), O_RDONLY);
    EXPECT_EQ(-1, fd);
    EXPECT_EQ(ENXIO, errno);
    if (fd >= 0) {
        close(fd);
    }

    unlink(manual_zero.c_str());
    unlink(block_zero.c_str());
    unlink(missing.c_str());
    ASSERT_EQ(0, umount(kMountPoint)) << "umount(" << kMountPoint << ") failed: errno=" << errno
                                      << " (" << strerror(errno) << ")";
}

TEST(DevtmpfsSemantics, TmpfsManualMknodUsesRegisteredDeviceNumber) {
    EnsureMountPoint(kTmpfsMountPoint);
    ASSERT_EQ(0, mount("tmpfs", kTmpfsMountPoint, "tmpfs", 0, "mode=0755"))
        << "mount(tmpfs) failed: errno=" << errno << " (" << strerror(errno) << ")";

    std::string manual_zero = std::string(kTmpfsMountPoint) + "/manual-zero";
    unlink(manual_zero.c_str());
    ASSERT_EQ(0, mknod(manual_zero.c_str(), S_IFCHR | 0600, makedev(1, 5)))
        << "mknod(" << manual_zero << ") failed: errno=" << errno << " (" << strerror(errno)
        << ")";
    ExpectCharDeviceNumber(manual_zero.c_str(), 1, 5);
    ExpectReadZeros(manual_zero.c_str());

    std::string missing = std::string(kTmpfsMountPoint) + "/manual-missing";
    unlink(missing.c_str());
    ASSERT_EQ(0, mknod(missing.c_str(), S_IFCHR | 0600, makedev(250, 250)))
        << "mknod(" << missing << ") failed: errno=" << errno << " (" << strerror(errno) << ")";

    errno = 0;
    int fd = open(missing.c_str(), O_RDONLY);
    EXPECT_EQ(-1, fd);
    EXPECT_EQ(ENXIO, errno);
    if (fd >= 0) {
        close(fd);
    }

    unlink(manual_zero.c_str());
    unlink(missing.c_str());
    ASSERT_EQ(0, umount(kTmpfsMountPoint))
        << "umount(" << kTmpfsMountPoint << ") failed: errno=" << errno << " ("
        << strerror(errno) << ")";
}

TEST(DevtmpfsSemantics, RejectsUnsupportedMountDataWithoutPollutingDev) {
    struct stat before = {};
    ASSERT_EQ(0, stat("/dev", &before)) << strerror(errno);

    EnsureMountPoint();
    errno = 0;
    EXPECT_EQ(-1, mount("devtmpfs", kMountPoint, "devtmpfs", 0, "badopt=1"));
    EXPECT_EQ(EINVAL, errno);

    struct stat after = {};
    ASSERT_EQ(0, stat("/dev", &after)) << strerror(errno);
    EXPECT_EQ(before.st_mode, after.st_mode);
    EXPECT_EQ(before.st_uid, after.st_uid);
    EXPECT_EQ(before.st_gid, after.st_gid);
}

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
