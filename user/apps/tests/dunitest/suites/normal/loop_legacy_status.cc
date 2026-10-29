#include <gtest/gtest.h>

#include <array>
#include <cerrno>
#include <climits>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <linux/fs.h>
#include <linux/loop.h>
#include <string>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>
#include <vector>

namespace {

#if defined(__x86_64__)
static_assert(sizeof(loop_info) == 168);
static_assert(offsetof(loop_info, lo_offset) == 32);
static_assert(offsetof(loop_info, lo_init) == 144);
static_assert(offsetof(loop_info, reserved) == 160);
#elif defined(__riscv) || defined(__loongarch64)
static_assert(sizeof(loop_info) == 160);
static_assert(offsetof(loop_info, lo_offset) == 20);
static_assert(offsetof(loop_info, lo_init) == 136);
static_assert(offsetof(loop_info, reserved) == 152);
#endif

constexpr uint64_t kBackingBytes = 1024 * 1024 + 333;

class LoopLegacyStatus : public testing::Test {
protected:
    void SetUp() override {
        const char* directory = getenv("DUNITEST_LOOP_LEGACY_BACKING_DIR");
        std::string pattern = std::string(directory ? directory : "/tmp") + "/loop_legacy_XXXXXX";
        std::vector<char> path(pattern.begin(), pattern.end());
        path.push_back('\0');
        backing_ = mkstemp(path.data());
        ASSERT_GE(backing_, 0) << strerror(errno);
        path_ = path.data();
        ASSERT_EQ(0, ftruncate(backing_, kBackingBytes));
        int control = open("/dev/loop-control", O_RDWR);
        ASSERT_GE(control, 0) << strerror(errno);
        const int minor = ioctl(control, LOOP_CTL_GET_FREE, 0);
        const int error = errno;
        close(control);
        ASSERT_GE(minor, 0) << strerror(error);
        minor_ = minor;
        loop_path_ = "/dev/loop" + std::to_string(minor);
        loop_ = open(loop_path_.c_str(), O_RDWR);
        ASSERT_GE(loop_, 0) << strerror(errno);
        ASSERT_EQ(0, ioctl(loop_, LOOP_SET_FD, backing_)) << strerror(errno);
        bound_ = true;
    }

    void TearDown() override {
        if (loop_ >= 0) {
            if (bound_) EXPECT_EQ(0, ioctl(loop_, LOOP_CLR_FD, 0)) << strerror(errno);
            close(loop_);
        }
        if (backing_ >= 0) close(backing_);
        if (!path_.empty()) unlink(path_.c_str());
    }

    void ExpectIdentity(const loop_info64& wide, const loop_info& legacy) {
        struct stat metadata {};
        ASSERT_EQ(0, fstat(backing_, &metadata));
        EXPECT_EQ(static_cast<uint64_t>(metadata.st_dev), wide.lo_device);
        EXPECT_EQ(static_cast<uint64_t>(metadata.st_ino), wide.lo_inode);
        EXPECT_EQ(static_cast<uint64_t>(metadata.st_rdev), wide.lo_rdevice);
        EXPECT_EQ(static_cast<unsigned>(minor_), wide.lo_number);
        EXPECT_EQ(wide.lo_device, static_cast<uint64_t>(legacy.lo_device));
        EXPECT_EQ(wide.lo_inode, static_cast<uint64_t>(legacy.lo_inode));
        EXPECT_EQ(wide.lo_rdevice, static_cast<uint64_t>(legacy.lo_rdevice));
        EXPECT_EQ(static_cast<int>(wide.lo_number), legacy.lo_number);
    }

    void ExpectMapping(uint64_t offset, uint64_t limit) {
        loop_info64 status {};
        ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS64, &status)) << strerror(errno);
        EXPECT_EQ(offset, status.lo_offset);
        EXPECT_EQ(limit, status.lo_sizelimit);
        uint64_t capacity = UINT64_MAX;
        ASSERT_EQ(0, ioctl(loop_, BLKGETSIZE64, &capacity));
        uint64_t expected = offset > kBackingBytes ? 0 : kBackingBytes - offset;
        if (limit != 0 && limit < expected) expected = limit;
        EXPECT_EQ(expected / 512 * 512, capacity);
    }

    int backing_ = -1;
    int loop_ = -1;
    int minor_ = -1;
    bool bound_ = false;
    std::string path_;
    std::string loop_path_;
};

TEST_F(LoopLegacyStatus, NativeGetInitializesPaddingReservedAndExactOutputSize) {
    loop_info64 wide {};
    ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS64, &wide));
    std::array<unsigned char, sizeof(loop_info) + 17> output;
    output.fill(0xA5);
    ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS, output.data() + 1)) << strerror(errno);
    loop_info legacy;
    memcpy(&legacy, output.data() + 1, sizeof(legacy));
    ASSERT_NO_FATAL_FAILURE(ExpectIdentity(wide, legacy));
    EXPECT_EQ(0, legacy.lo_offset);
    EXPECT_EQ(0, legacy.lo_encrypt_type);
    EXPECT_EQ(0, legacy.lo_encrypt_key_size);
    for (unsigned char byte : legacy.lo_encrypt_key) EXPECT_EQ(0, byte);
    for (unsigned long word : legacy.lo_init) EXPECT_EQ(0UL, word);
    for (char byte : legacy.reserved) EXPECT_EQ(0, byte);
#if defined(__x86_64__)
    for (size_t index : {4U, 5U, 6U, 7U, 164U, 165U, 166U, 167U}) {
#else
    for (size_t index : {132U, 133U, 134U, 135U, 156U, 157U, 158U, 159U}) {
#endif
        EXPECT_EQ(0, output[index + 1]) << "padding byte " << index;
    }
    EXPECT_EQ(0xA5, output[0]);
    for (size_t index = sizeof(loop_info) + 1; index < output.size(); ++index) {
        EXPECT_EQ(0xA5, output[index]);
    }
}

TEST_F(LoopLegacyStatus, LegacySetAcceptsByteOffsetAndClearsWideSizeLimit) {
    loop_info64 wide {};
    wide.lo_sizelimit = 4096;
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS64, &wide));
    loop_info legacy {};
    legacy.lo_offset = 513;
    legacy.lo_flags = LO_FLAGS_AUTOCLEAR;
    legacy.lo_device = static_cast<decltype(legacy.lo_device)>(~0UL);
    legacy.lo_inode = ~0UL;
    legacy.lo_rdevice = static_cast<decltype(legacy.lo_rdevice)>(~0UL);
    legacy.lo_number = INT_MAX;
    memcpy(legacy.lo_name, "legacy-name", 12);
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS, &legacy)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectMapping(513, 0));
    memset(&wide, 0, sizeof(wide));
    ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS64, &wide));
    EXPECT_EQ(static_cast<unsigned>(LO_FLAGS_AUTOCLEAR), wide.lo_flags);
    EXPECT_STREQ("legacy-name", reinterpret_cast<const char*>(wide.lo_file_name));
    memset(&legacy, 0, sizeof(legacy));
    ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS, &legacy));
    EXPECT_EQ(513, legacy.lo_offset);
    EXPECT_STREQ("legacy-name", legacy.lo_name);
    ASSERT_NO_FATAL_FAILURE(ExpectIdentity(wide, legacy));
}

TEST_F(LoopLegacyStatus, LegacyGetOverflowDoesNotCopyOutputOrChangeMapping) {
    loop_info64 status {};
    status.lo_offset = uint64_t(INT_MAX) + 1;
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS64, &status));
    std::array<unsigned char, sizeof(loop_info)> output;
    output.fill(0xA5);
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_, LOOP_GET_STATUS, output.data()));
    EXPECT_EQ(EOVERFLOW, errno);
    for (unsigned char byte : output) EXPECT_EQ(0xA5, byte);
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_, LOOP_GET_STATUS, reinterpret_cast<void*>(1)));
    EXPECT_EQ(EOVERFLOW, errno) << "conversion precedes user copy";
    ASSERT_NO_FATAL_FAILURE(ExpectMapping(status.lo_offset, 0));
}

TEST_F(LoopLegacyStatus, NegativeLegacyOffsetOverflowsWithoutMutation) {
    loop_info status {};
    status.lo_offset = -1;
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_, LOOP_SET_STATUS, &status));
    EXPECT_EQ(EOVERFLOW, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectMapping(0, 0));
}

TEST_F(LoopLegacyStatus, LegacyIgnoresObsoleteCryptoButWideStatusRejectsUnsupportedCrypto) {
    loop_info legacy {};
    legacy.lo_offset = 513;
    legacy.lo_encrypt_type = INT_MAX;
    legacy.lo_encrypt_key_size = INT_MAX;
    memset(legacy.lo_encrypt_key, 0xA5, sizeof(legacy.lo_encrypt_key));
    legacy.lo_init[0] = ~0UL;
    legacy.lo_init[1] = ~0UL;
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS, &legacy)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectMapping(513, 0));
    loop_info64 wide {};
    wide.lo_offset = 1025;
    wide.lo_encrypt_type = 1;
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_, LOOP_SET_STATUS64, &wide));
    EXPECT_EQ(EINVAL, errno);
    wide.lo_encrypt_type = 0;
    wide.lo_encrypt_key_size = 33;
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_, LOOP_SET_STATUS64, &wide));
    EXPECT_EQ(EINVAL, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectMapping(513, 0));
    memset(&wide, 0xA5, sizeof(wide));
    ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS64, &wide));
    EXPECT_EQ(0U, wide.lo_encrypt_type);
    EXPECT_EQ(0U, wide.lo_encrypt_key_size);
    for (unsigned char byte : wide.lo_encrypt_key) EXPECT_EQ(0, byte);
    for (uint64_t word : wide.lo_init) EXPECT_EQ(0U, word);
}

TEST_F(LoopLegacyStatus, StatusNameIsTerminatedAndReadonlyFlagIsNotSettable) {
    loop_info legacy {};
    legacy.lo_flags = LO_FLAGS_READ_ONLY;
    memset(legacy.lo_name, 'A', sizeof(legacy.lo_name));
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS, &legacy));
    loop_info64 status {};
    ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS64, &status));
    EXPECT_EQ(0U, status.lo_flags & LO_FLAGS_READ_ONLY);
    for (unsigned index = 0; index != 63; ++index) EXPECT_EQ('A', status.lo_file_name[index]);
    EXPECT_EQ(0, status.lo_file_name[63]);
    // Unknown bits do not grant features: Linux masks them on publication.
    legacy.lo_flags = 1U << 30;
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS, &legacy));
    ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS64, &status));
    EXPECT_EQ(0U, status.lo_flags);
}

TEST_F(LoopLegacyStatus, LegacyStatusCannotClearAReadonlyBinding) {
    ASSERT_EQ(0, ioctl(loop_, LOOP_CLR_FD, 0));
    bound_ = false;
    int read_only = open(path_.c_str(), O_RDONLY);
    ASSERT_GE(read_only, 0);
    const int result = ioctl(loop_, LOOP_SET_FD, read_only);
    const int error = errno;
    close(read_only);
    ASSERT_EQ(0, result) << strerror(error);
    bound_ = true;
    loop_info status {};
    status.lo_flags = LO_FLAGS_AUTOCLEAR;
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS, &status));
    ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS, &status));
    EXPECT_EQ(LO_FLAGS_READ_ONLY | LO_FLAGS_AUTOCLEAR, status.lo_flags);
    status.lo_flags = 0;
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS, &status));
    ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS, &status));
    EXPECT_EQ(LO_FLAGS_READ_ONLY, status.lo_flags);
}

TEST_F(LoopLegacyStatus, UnprivilegedReadonlyCallerMayGetButCannotSetStatus) {
    int read_only = open(loop_path_.c_str(), O_RDONLY);
    ASSERT_GE(read_only, 0);
    const pid_t child = fork();
    if (child < 0) {
        const int error = errno;
        close(read_only);
        FAIL() << strerror(error);
    }
    if (child == 0) {
        if (setgid(65534) || setuid(65534)) _exit(1);
        errno = 0;
        if (ioctl(read_only, LOOP_SET_STATUS, nullptr) != -1 || errno != EPERM) _exit(2);
        loop_info status {};
        _exit(ioctl(read_only, LOOP_GET_STATUS, &status) == 0 ? 0 : 3);
    }
    close(read_only);
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST_F(LoopLegacyStatus, UserPointerAndUnboundStateErrorOrderingMatchesLinux) {
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_, LOOP_SET_STATUS, nullptr));
    EXPECT_EQ(EFAULT, errno);
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_, LOOP_GET_STATUS, nullptr));
    EXPECT_EQ(EINVAL, errno);
    const long page = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page, 0);
    void* unavailable = mmap(nullptr, page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, unavailable);
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_, LOOP_GET_STATUS, unavailable));
    EXPECT_EQ(EFAULT, errno);
    ASSERT_EQ(0, munmap(unavailable, page));
    ASSERT_EQ(0, ioctl(loop_, LOOP_CLR_FD, 0));
    bound_ = false;
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_, LOOP_SET_STATUS, reinterpret_cast<void*>(1)));
    EXPECT_EQ(EFAULT, errno);
    loop_info invalid {};
    invalid.lo_offset = -1;
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_, LOOP_SET_STATUS, &invalid));
    EXPECT_EQ(ENXIO, errno) << "bound-state validation precedes field validation";
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_, LOOP_GET_STATUS, reinterpret_cast<void*>(1)));
    EXPECT_EQ(ENXIO, errno);
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_, LOOP_GET_STATUS, nullptr));
    EXPECT_EQ(EINVAL, errno);
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
