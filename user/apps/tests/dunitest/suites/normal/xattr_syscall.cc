#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <string>
#include <vector>

namespace {
constexpr char kName[] = "user.syscall_buffer";
constexpr char kValue[] = "original attribute bytes";
constexpr size_t kMax = 65536;
enum class Access { Path, Fd, NoFollow };
constexpr std::array<Access, 3> kAccess = {Access::Path, Access::Fd, Access::NoFollow};

class XattrSyscall : public ::testing::Test {
  protected:
    void SetUp() override {
        struct statfs st = {};
        ASSERT_EQ(0, statfs("/root", &st));
        if (st.f_type != 0xEF53) {
            GTEST_SKIP() << "/root must be ext4";
        }
        char pattern[] = "/root/dunitest_xattr_syscall_XXXXXX";
        fd = mkstemp(pattern);
        ASSERT_GE(fd, 0) << strerror(errno);
        path = pattern;
        ASSERT_EQ(0, Set(Access::Path, kValue, sizeof(kValue))) << strerror(errno);
    }
    void TearDown() override {
        if (fd >= 0) close(fd);
        if (!path.empty()) unlink(path.c_str());
    }
    // Raw syscalls intentionally test invalid pointers without libc access annotations.
    int Set(Access mode, const void* value, size_t size, int flags = 0) {
        if (mode == Access::Fd) return syscall(SYS_fsetxattr, fd, kName, value, size, flags);
        if (mode == Access::NoFollow) return syscall(SYS_lsetxattr, path.c_str(), kName, value, size, flags);
        return syscall(SYS_setxattr, path.c_str(), kName, value, size, flags);
    }
    ssize_t Get(Access mode, void* value, size_t size) {
        if (mode == Access::Fd) return syscall(SYS_fgetxattr, fd, kName, value, size);
        if (mode == Access::NoFollow) return syscall(SYS_lgetxattr, path.c_str(), kName, value, size);
        return syscall(SYS_getxattr, path.c_str(), kName, value, size);
    }
    ssize_t List(Access mode, void* value, size_t size) {
        if (mode == Access::Fd) return syscall(SYS_flistxattr, fd, value, size);
        if (mode == Access::NoFollow)
            return syscall(SYS_llistxattr, path.c_str(), value, size);
        return syscall(SYS_listxattr, path.c_str(), value, size);
    }
    void ExpectOriginal() {
        std::array<char, sizeof(kValue)> value = {};
        ASSERT_EQ(static_cast<ssize_t>(value.size()), Get(Access::Path, value.data(), value.size()));
        EXPECT_EQ(0, memcmp(kValue, value.data(), value.size()));
    }
    int fd = -1;
    std::string path;
};

TEST_F(XattrSyscall, ReadOnlyOutputReturnsEFAULT) {
    const size_t page = sysconf(_SC_PAGESIZE);
    void* output = mmap(nullptr, page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, output);
    memset(output, 0x5a, page);
    ASSERT_EQ(0, mprotect(output, page, PROT_READ));
    for (Access mode : kAccess) {
        errno = 0;
        EXPECT_EQ(-1, Get(mode, output, kMax));
        EXPECT_EQ(EFAULT, errno);
        errno = 0;
        EXPECT_EQ(-1, List(mode, output, kMax));
        EXPECT_EQ(EFAULT, errno);
    }
    EXPECT_EQ(0, munmap(output, page));
    ExpectOriginal();
}
TEST_F(XattrSyscall, OutputCapacityCopiesOnlyActualBytes) {
    const std::array<size_t, 10> capacities = {sizeof(kValue), 2048, 2049, 4096, 4097,
                                               8192, 8193, 65535, kMax, kMax + 1};
    for (Access mode : kAccess) {
        SCOPED_TRACE(static_cast<int>(mode));
        const ssize_t needed = List(mode, nullptr, 0);
        ASSERT_GT(needed, 0);
        for (size_t capacity : capacities) {
            SCOPED_TRACE(capacity);
            std::vector<unsigned char> output(capacity + 16, 0xa7);
            ASSERT_EQ(static_cast<ssize_t>(sizeof(kValue)), Get(mode, output.data(), capacity));
            EXPECT_EQ(0, memcmp(kValue, output.data(), sizeof(kValue)));
            EXPECT_TRUE(std::all_of(output.begin() + sizeof(kValue), output.end(),
                                    [](unsigned char byte) { return byte == 0xa7; }));
            std::fill(output.begin(), output.end(), 0xa7);
            ASSERT_EQ(needed, List(mode, output.data(), capacity));
            EXPECT_EQ(0, memcmp(kName, output.data(), sizeof(kName)));
            EXPECT_TRUE(std::all_of(output.begin() + needed, output.end(),
                                    [](unsigned char byte) { return byte == 0xa7; }));
        }
        std::array<unsigned char, sizeof(kValue)> small;
        small.fill(0xa7);
        errno = 0;
        EXPECT_EQ(-1, Get(mode, small.data(), sizeof(kValue) - 1));
        EXPECT_EQ(ERANGE, errno);
        EXPECT_TRUE(std::all_of(small.begin(), small.end(),
                                [](unsigned char byte) { return byte == 0xa7; }));
        errno = 0;
        EXPECT_EQ(-1, List(mode, small.data(), needed - 1));
        EXPECT_EQ(ERANGE, errno);
        EXPECT_TRUE(std::all_of(small.begin(), small.end(),
                                [](unsigned char byte) { return byte == 0xa7; }));
    }
}

TEST_F(XattrSyscall, EmptyResultsIgnoreInvalidPointers) {
    void* bad = reinterpret_cast<void*>(1);
    for (Access mode : kAccess) {
        EXPECT_EQ(static_cast<ssize_t>(sizeof(kValue)), Get(mode, bad, 0));
        EXPECT_EQ(static_cast<ssize_t>(sizeof(kName)), List(mode, bad, 0));
        ASSERT_EQ(0, Set(mode, bad, 0));
        EXPECT_EQ(0, Get(mode, nullptr, kMax));
        ASSERT_EQ(0, Set(mode, kValue, sizeof(kValue)));
    }
    ASSERT_EQ(0, removexattr(path.c_str(), kName));
    for (Access mode : kAccess) EXPECT_EQ(0, List(mode, nullptr, kMax));
}

TEST_F(XattrSyscall, InputBoundariesFlagsAndDescriptorPrecedence) {
    std::vector<unsigned char> input(kMax + 1);
    for (size_t index = 0; index < input.size(); ++index) input[index] = index * 37;
    for (Access mode : kAccess) {
        for (size_t size : {size_t(0), size_t(1), size_t(2048)}) {
            ASSERT_EQ(0, Set(mode, input.data(), size));
            std::vector<unsigned char> output(size + 1, 0xa7);
            ASSERT_EQ(static_cast<ssize_t>(size), Get(mode, output.data(), output.size()));
            EXPECT_TRUE(std::equal(input.begin(), input.begin() + size, output.begin()));
            EXPECT_EQ(0xa7, output[size]);
        }
        ASSERT_EQ(0, Set(mode, kValue, sizeof(kValue)));
        errno = 0;
        EXPECT_EQ(-1, Set(mode, reinterpret_cast<void*>(1), kMax + 1));
        EXPECT_EQ(E2BIG, errno);
        errno = 0;
        EXPECT_EQ(-1, Set(mode, reinterpret_cast<void*>(1), 1, 0x4000));
        EXPECT_EQ(EINVAL, errno);
        errno = 0;
        EXPECT_EQ(-1, Set(mode, input.data(), 1, XATTR_CREATE));
        EXPECT_EQ(EEXIST, errno);
        ExpectOriginal();
        // ext4 with ea_inode may support this; single-block ext4 rejects it.
        errno = 0;
        const int result = Set(mode, input.data(), kMax, XATTR_REPLACE);
        if (result == 0) {
            std::vector<unsigned char> output(kMax);
            ASSERT_EQ(static_cast<ssize_t>(kMax), Get(mode, output.data(), output.size()));
            EXPECT_TRUE(std::equal(output.begin(), output.end(), input.begin()));
            ASSERT_EQ(0, Set(mode, kValue, sizeof(kValue)));
        } else {
            EXPECT_TRUE(errno == ENOSPC || errno == E2BIG) << strerror(errno);
            ExpectOriginal();
        }
    }
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_fsetxattr, -1, nullptr, nullptr, kMax + 1, 0x4000));
    EXPECT_EQ(EBADF, errno);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_fgetxattr, -1, nullptr, nullptr, kMax));
    EXPECT_EQ(EBADF, errno);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_flistxattr, -1, nullptr, kMax));
    EXPECT_EQ(EBADF, errno);
}

TEST_F(XattrSyscall, NameAndPathErrorsRetainLinuxPrecedence) {
    const std::string missing = path + ".missing";
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_setxattr, missing.c_str(), "", nullptr, 0, 0));
    EXPECT_EQ(ERANGE, errno);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_setxattr, missing.c_str(), nullptr, nullptr, 0, 0x4000));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_getxattr, missing.c_str(), nullptr, nullptr, 0));
    EXPECT_EQ(ENOENT, errno);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_fsetxattr, fd, nullptr, nullptr, 0, 0));
    EXPECT_EQ(EFAULT, errno);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_fgetxattr, fd, "", nullptr, 0));
    EXPECT_EQ(ERANGE, errno);
    ExpectOriginal();
}

TEST_F(XattrSyscall, ProtectedCopyFaultsPreserveAttributeAndAllowRecovery) {
    const size_t page = sysconf(_SC_PAGESIZE);
    auto* mapping = static_cast<unsigned char*>(
        mmap(nullptr, page * 2, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, mapping);
    memset(mapping, 0xa7, page * 2);
    ASSERT_EQ(0, mprotect(mapping + page, page, PROT_NONE));
    for (Access mode : kAccess) {
        for (void* bad : {static_cast<void*>(mapping + page),
                          static_cast<void*>(mapping + page - 4)}) {
            errno = 0;
            EXPECT_EQ(-1, Set(mode, bad, sizeof(kValue)));
            EXPECT_EQ(EFAULT, errno);
            ExpectOriginal();
            errno = 0;
            EXPECT_EQ(-1, Get(mode, bad, kMax));
            EXPECT_EQ(EFAULT, errno);
            errno = 0;
            EXPECT_EQ(-1, List(mode, bad, kMax));
            EXPECT_EQ(EFAULT, errno);
            char output[128] = {};
            EXPECT_EQ(static_cast<ssize_t>(sizeof(kValue)), Get(mode, output, sizeof(output)));
            EXPECT_EQ(static_cast<ssize_t>(sizeof(kName)), List(mode, output, sizeof(output)));
        }
        errno = 0;
        EXPECT_EQ(-1, getxattr(path.c_str(), "user.missing", mapping + page, kMax));
        EXPECT_EQ(ENODATA, errno);
    }
    EXPECT_EQ(0, munmap(mapping, page * 2));
}

TEST_F(XattrSyscall, PrivateOutputHandlesCopyOnWrite) {
    const size_t page = sysconf(_SC_PAGESIZE);
    auto* output = static_cast<unsigned char*>(
        mmap(nullptr, page * 2, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, output);
    memset(output, 0xa7, page * 2);
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        for (Access mode : kAccess) {
            if (Get(mode, output, kMax) != sizeof(kValue)
                || memcmp(output, kValue, sizeof(kValue)) != 0
                || List(mode, output + page, kMax) != sizeof(kName)
                || memcmp(output + page, kName, sizeof(kName)) != 0) {
                _exit(1);
            }
        }
        _exit(0);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_TRUE(std::all_of(output, output + page * 2,
                            [](unsigned char byte) { return byte == 0xa7; }));
    EXPECT_EQ(0, munmap(output, page * 2));
}

TEST_F(XattrSyscall, ResidentNonePageSurvivesSiblingUnmapAndRestoresBytes) {
    const size_t page = sysconf(_SC_PAGESIZE);
    constexpr size_t leaf_span = 2 * 1024 * 1024;
    auto* reservation = static_cast<unsigned char*>(
        mmap(nullptr, leaf_span * 2, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, reservation);
    // Two touched entries in one otherwise untouched leaf table. Removing the
    // accessible entry must not reclaim the table containing the NONE entry.
    const uintptr_t aligned = (reinterpret_cast<uintptr_t>(reservation) + leaf_span - 1)
                              & ~(leaf_span - 1);
    auto* resident = reinterpret_cast<unsigned char*>(aligned);
    memset(resident, 0x47, page);
    memset(resident + page, 0x39, page);
    ASSERT_EQ(0, mprotect(resident, page, PROT_NONE));
    unsigned char present = 0;
    ASSERT_EQ(0, mincore(resident, page, &present));
    EXPECT_EQ(1, present & 1);
    for (Access mode : kAccess) {
        errno = 0;
        EXPECT_EQ(-1, Set(mode, resident, sizeof(kValue)));
        EXPECT_EQ(EFAULT, errno);
        ExpectOriginal();
    }
    ASSERT_EQ(0, munmap(resident + page, page));
    ASSERT_EQ(0, mincore(resident, page, &present));
    EXPECT_EQ(1, present & 1);
    ASSERT_EQ(0, mprotect(resident, page, PROT_READ | PROT_WRITE));
    EXPECT_TRUE(std::all_of(resident, resident + page,
                            [](unsigned char byte) { return byte == 0x47; }));
    ASSERT_EQ(0, mprotect(resident, page, PROT_NONE));
    ASSERT_EQ(0, munmap(resident, page));
    ASSERT_EQ(resident, mmap(resident, page, PROT_READ | PROT_WRITE,
                            MAP_FIXED | MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    EXPECT_TRUE(std::all_of(resident, resident + page,
                            [](unsigned char byte) { return byte == 0; }));
    EXPECT_EQ(0, munmap(reservation, leaf_span * 2));
}

TEST_F(XattrSyscall, ForkRetainsNonePagesAndRestoredWritesUseCopyOnWrite) {
    const size_t page = sysconf(_SC_PAGESIZE);
    auto* resident = static_cast<unsigned char*>(
        mmap(nullptr, page * 2, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, resident);
    memset(resident, 0x63, page * 2);
    ASSERT_EQ(0, mprotect(resident, page * 2, PROT_NONE));
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        unsigned char present[2] = {};
        if (mincore(resident, page * 2, present) != 0
            || !(present[0] & 1) || !(present[1] & 1)
            || mprotect(resident, page, PROT_READ | PROT_WRITE) != 0
            || !std::all_of(resident, resident + page,
                            [](unsigned char byte) { return byte == 0x63; })) {
            _exit(1);
        }
        memset(resident, 0x91, page);
        // The second inherited resident page remains NONE through child exit.
        _exit(0);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));
    ASSERT_EQ(0, mprotect(resident, page * 2, PROT_READ | PROT_WRITE));
    EXPECT_TRUE(std::all_of(resident, resident + page * 2,
                            [](unsigned char byte) { return byte == 0x63; }));
    ASSERT_EQ(0, mprotect(resident, page * 2, PROT_NONE));
    EXPECT_EQ(0, munmap(resident, page * 2));
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
