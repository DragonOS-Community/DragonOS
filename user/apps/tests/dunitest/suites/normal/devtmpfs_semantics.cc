#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/wait.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <unistd.h>

#include <cstdint>
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

TEST(DevtmpfsSemantics, StatfsRejectsInvalidPathPointer) {
    struct statfs st = {};
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_statfs, reinterpret_cast<const char*>(1), &st));
    EXPECT_EQ(EFAULT, errno);
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

TEST(DevtmpfsSemantics, PrivateZeroMmapPreservesPagesAcrossFaultAroundWindows) {
    constexpr size_t kPageSize = 4096;
    constexpr size_t kPages = 33;
    constexpr size_t kLength = kPages * kPageSize;
    // 32 divides the 512-entry x86 PTE table. Offset 5 leaves enough room for
    // two complete 16-page fault-around windows without crossing its end.
    constexpr size_t kAlignmentPages = 32;
    constexpr size_t kReservePages = kPages + kAlignmentPages - 1;
    constexpr uintptr_t kDesiredPteOffset = 5;

    int fd = open("/dev/zero", O_RDWR);
    ASSERT_GE(fd, 0) << strerror(errno);

    void* reservation = mmap(nullptr, kReservePages * kPageSize, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE, fd, 0);
    if (reservation == MAP_FAILED) {
        const int saved_errno = errno;
        close(fd);
        FAIL() << strerror(saved_errno);
        return;
    }
    const uintptr_t first_page = reinterpret_cast<uintptr_t>(reservation) / kPageSize;
    const size_t prefix_pages = (kDesiredPteOffset + kAlignmentPages -
                                 (first_page & (kAlignmentPages - 1))) &
                                (kAlignmentPages - 1);
    const size_t suffix_pages = kReservePages - prefix_pages - kPages;
    auto* raw_mapping = static_cast<unsigned char*>(reservation) + prefix_pages * kPageSize;
    if (prefix_pages != 0 && munmap(reservation, prefix_pages * kPageSize) != 0) {
        const int saved_errno = errno;
        munmap(reservation, kReservePages * kPageSize);
        close(fd);
        FAIL() << strerror(saved_errno);
        return;
    }
    if (suffix_pages != 0 && munmap(raw_mapping + kLength, suffix_pages * kPageSize) != 0) {
        const int saved_errno = errno;
        munmap(raw_mapping, (kPages + suffix_pages) * kPageSize);
        close(fd);
        FAIL() << strerror(saved_errno);
        return;
    }
    EXPECT_EQ(kDesiredPteOffset,
              (reinterpret_cast<uintptr_t>(raw_mapping) / kPageSize) & (kAlignmentPages - 1));
    auto* mapping = static_cast<volatile unsigned char*>(raw_mapping);

    // A deliberately unaligned VMA makes consecutive fault-around windows
    // overlap. Later faults must preserve pages populated by an earlier window.
    for (size_t page = 0; page < kPages; ++page) {
        EXPECT_EQ(0, mapping[page * kPageSize]);
        mapping[page * kPageSize] = static_cast<unsigned char>(page + 1);
    }
    for (size_t page = 0; page < kPages; ++page) {
        EXPECT_EQ(static_cast<unsigned char>(page + 1), mapping[page * kPageSize]);
    }
    EXPECT_EQ(0, munmap(const_cast<unsigned char*>(mapping), kLength));

    void* raw_populated =
        mmap(nullptr, kLength, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_POPULATE, fd, 0);
    if (raw_populated == MAP_FAILED) {
        const int saved_errno = errno;
        close(fd);
        FAIL() << strerror(saved_errno);
        return;
    }
    auto* populated = static_cast<volatile unsigned char*>(raw_populated);
    for (size_t page = 0; page < kPages; ++page) {
        EXPECT_EQ(0, populated[page * kPageSize]);
        populated[page * kPageSize] = static_cast<unsigned char>(0xa0 + page);
    }
    for (size_t page = 0; page < kPages; ++page) {
        EXPECT_EQ(static_cast<unsigned char>(0xa0 + page), populated[page * kPageSize]);
    }
    EXPECT_EQ(0, munmap(const_cast<unsigned char*>(populated), kLength));
    EXPECT_EQ(0, close(fd));
}

TEST(DevtmpfsSemantics, SharedZeroLazyFaultsRemainSharedAcrossFork) {
    constexpr size_t kPageSize = 4096;
    int fd = open("/dev/zero", O_RDWR);
    ASSERT_GE(fd, 0) << strerror(errno);
    auto* mapping = static_cast<volatile unsigned char*>(
        mmap(nullptr, 2 * kPageSize, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0));
    ASSERT_NE(MAP_FAILED, const_cast<unsigned char*>(mapping)) << strerror(errno);

    int parent_to_child[2];
    int child_to_parent[2];
    ASSERT_EQ(0, pipe(parent_to_child)) << strerror(errno);
    ASSERT_EQ(0, pipe(child_to_parent)) << strerror(errno);
    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        close(parent_to_child[1]);
        close(child_to_parent[0]);
        unsigned char token = 0;
        if (read(parent_to_child[0], &token, 1) != 1 || mapping[0] != 0x31) {
            _exit(1);
        }
        mapping[kPageSize] = 0x52;
        token = 1;
        if (write(child_to_parent[1], &token, 1) != 1) {
            _exit(2);
        }
        _exit(0);
    }

    close(parent_to_child[0]);
    close(child_to_parent[1]);
    mapping[0] = 0x31;
    unsigned char token = 1;
    ASSERT_EQ(1, write(parent_to_child[1], &token, 1));
    ASSERT_EQ(1, read(child_to_parent[0], &token, 1));

    // The child instantiated page 1. It must be resident in the common
    // backing even though this VMA does not yet have a PTE for it.
    unsigned char residency[2] = {};
    ASSERT_EQ(0, mincore(const_cast<unsigned char*>(mapping), 2 * kPageSize, residency))
        << strerror(errno);
    EXPECT_NE(0, residency[1] & 1);
    EXPECT_EQ(0x52, mapping[kPageSize]);

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    EXPECT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_EQ(0, close(parent_to_child[1]));
    EXPECT_EQ(0, close(child_to_parent[0]));
    EXPECT_EQ(0, munmap(const_cast<unsigned char*>(mapping), 2 * kPageSize));
    EXPECT_EQ(0, close(fd));
}

TEST(DevtmpfsSemantics, SharedZeroMappingsHavePerMmapIdentity) {
    constexpr size_t kPageSize = 4096;
    int first_fd = open("/dev/zero", O_RDWR);
    int second_fd = open("/dev/zero", O_RDWR);
    ASSERT_GE(first_fd, 0) << strerror(errno);
    ASSERT_GE(second_fd, 0) << strerror(errno);
    auto* first = static_cast<unsigned char*>(
        mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE, MAP_SHARED, first_fd, 0));
    auto* same_fd = static_cast<unsigned char*>(
        mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE, MAP_SHARED, first_fd, 0));
    auto* other_fd = static_cast<unsigned char*>(
        mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE, MAP_SHARED, second_fd, 0));
    ASSERT_NE(MAP_FAILED, first) << strerror(errno);
    ASSERT_NE(MAP_FAILED, same_fd) << strerror(errno);
    ASSERT_NE(MAP_FAILED, other_fd) << strerror(errno);

    first[0] = 0xa5;
    EXPECT_EQ(0, same_fd[0]);
    EXPECT_EQ(0, other_fd[0]);
    EXPECT_EQ(0, msync(first, kPageSize, MS_SYNC)) << strerror(errno);
    EXPECT_EQ(0, msync(first, kPageSize, MS_ASYNC)) << strerror(errno);

    EXPECT_EQ(0, munmap(first, kPageSize));
    EXPECT_EQ(0, munmap(same_fd, kPageSize));
    EXPECT_EQ(0, munmap(other_fd, kPageSize));
    EXPECT_EQ(0, close(first_fd));
    EXPECT_EQ(0, close(second_fd));
}

TEST(DevtmpfsSemantics, SharedZeroUsesBackingIdentityForFutex) {
    constexpr size_t kPageSize = 4096;
    int fd = open("/dev/zero", O_RDWR);
    ASSERT_GE(fd, 0) << strerror(errno);
    auto* futex_word = static_cast<int*>(
        mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0));
    ASSERT_NE(MAP_FAILED, futex_word) << strerror(errno);
    *futex_word = 0;

    int ready[2];
    ASSERT_EQ(0, pipe(ready)) << strerror(errno);
    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        close(ready[0]);
        unsigned char token = 1;
        if (write(ready[1], &token, 1) != 1) {
            _exit(1);
        }
        int ret = syscall(SYS_futex, futex_word, FUTEX_WAIT, 0, nullptr, nullptr, 0);
        _exit(ret == 0 ? 0 : 2);
    }

    close(ready[1]);
    unsigned char token = 0;
    ASSERT_EQ(1, read(ready[0], &token, 1));
    int wake_count = 0;
    for (int attempt = 0; attempt < 10000 && wake_count == 0; ++attempt) {
        wake_count = syscall(SYS_futex, futex_word, FUTEX_WAKE, 1, nullptr, nullptr, 0);
        if (wake_count == 0) {
            sched_yield();
        }
    }
    if (wake_count != 1) {
        kill(child, SIGKILL);
    }
    EXPECT_EQ(1, wake_count);
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    EXPECT_TRUE(WIFEXITED(status));
    if (WIFEXITED(status)) {
        EXPECT_EQ(0, WEXITSTATUS(status));
    }
    EXPECT_EQ(0, close(ready[0]));
    EXPECT_EQ(0, munmap(futex_word, kPageSize));
    EXPECT_EQ(0, close(fd));
}

TEST(DevtmpfsSemantics, SharedZeroFaultAroundDoesNotAllocateColdPages) {
    constexpr size_t kPageSize = 4096;
    constexpr size_t kPages = 33;
    int fd = open("/dev/zero", O_RDWR);
    ASSERT_GE(fd, 0) << strerror(errno);
    auto* mapping = static_cast<volatile unsigned char*>(
        mmap(nullptr, kPages * kPageSize, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0));
    ASSERT_NE(MAP_FAILED, const_cast<unsigned char*>(mapping)) << strerror(errno);

    EXPECT_EQ(0, mapping[16 * kPageSize]);
    unsigned char residency[kPages] = {};
    ASSERT_EQ(0, mincore(const_cast<unsigned char*>(mapping), kPages * kPageSize, residency))
        << strerror(errno);
    for (size_t page = 0; page < kPages; ++page) {
        EXPECT_EQ(page == 16, (residency[page] & 1) != 0) << "page=" << page;
    }

    EXPECT_EQ(0, munmap(const_cast<unsigned char*>(mapping), kPages * kPageSize));
    EXPECT_EQ(0, close(fd));
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
