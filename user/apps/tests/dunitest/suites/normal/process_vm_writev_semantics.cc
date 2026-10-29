#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <cerrno>
#include <algorithm>
#include <cstdlib>
#include <cstring>
#include <vector>

#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

namespace {

ssize_t write_self(void* destination, const void* source, size_t length) {
    iovec local = {};
    local.iov_base = const_cast<void*>(source);
    local.iov_len = length;
    iovec remote = {};
    remote.iov_base = destination;
    remote.iov_len = length;
    return syscall(SYS_process_vm_writev, getpid(), &local, 1, &remote, 1, 0);
}

TEST(ProcessVmTest, FlagsAreValidatedBeforeIovecPointers) {
    errno = 0;
    EXPECT_EQ(syscall(SYS_process_vm_writev, 0,
                      reinterpret_cast<const iovec*>(1), 1,
                      reinterpret_cast<const iovec*>(1), 1, 1),
              -1);
    EXPECT_EQ(errno, EINVAL);
}

TEST(ProcessVmTest, InvalidLocalIovecPrecedesDeadPid) {
    iovec remote = {};
    unsigned char byte = 0;
    remote.iov_base = &byte;
    remote.iov_len = 1;

    errno = 0;
    EXPECT_EQ(syscall(SYS_process_vm_readv, 0,
                      reinterpret_cast<const iovec*>(1), 1, &remote, 1, 0),
              -1);
    EXPECT_EQ(errno, EFAULT);
}

TEST(ProcessVmTest, IovecLengthMustFitSsizeBeforeAnyTransfer) {
    unsigned char byte = 0;
    iovec valid = {};
    valid.iov_base = &byte;
    valid.iov_len = 1;
    iovec oversized = valid;
    oversized.iov_len = static_cast<size_t>(INTPTR_MAX) + 1;

    errno = 0;
    EXPECT_EQ(syscall(SYS_process_vm_readv, getpid(), &oversized, 1, &valid,
                      1, 0),
              -1);
    EXPECT_EQ(errno, EINVAL);

    errno = 0;
    EXPECT_EQ(syscall(SYS_process_vm_readv, getpid(), &valid, 1, &oversized,
                      1, 0),
              -1);
    EXPECT_EQ(errno, EINVAL);
}

TEST(ProcessVmTest, TruncatedLocalTailIsStillRangeValidated) {
    constexpr size_t kMaxRwCount =
        static_cast<size_t>(INT32_MAX) & ~static_cast<size_t>(4095);
    iovec local[2] = {};
    local[0].iov_base = reinterpret_cast<void*>(0x10000);
    local[0].iov_len = kMaxRwCount;
    local[1].iov_base = reinterpret_cast<void*>(UINTPTR_MAX - 1);
    local[1].iov_len = 2;
    unsigned char byte = 0;
    iovec remote = {};
    remote.iov_base = &byte;
    remote.iov_len = 1;

    errno = 0;
    EXPECT_EQ(syscall(SYS_process_vm_readv, 0, local, 2, &remote, 1, 0), -1);
    EXPECT_EQ(errno, EFAULT);
}

TEST(ProcessVmTest, EmptyLocalIteratorSkipsRemoteValidationAndPidLookup) {
    unsigned char byte = 0;
    iovec local = {};
    local.iov_base = &byte;
    local.iov_len = 0;

    errno = 0;
    EXPECT_EQ(syscall(SYS_process_vm_writev, 0, &local, 1,
                      reinterpret_cast<const iovec*>(1), 1025, 0),
              0);
}

TEST(ProcessVmTest, InvalidRemoteIovecPrecedesDeadPid) {
    unsigned char byte = 0;
    iovec local = {};
    local.iov_base = &byte;
    local.iov_len = 1;

    errno = 0;
    EXPECT_EQ(syscall(SYS_process_vm_writev, 0, &local, 1,
                      reinterpret_cast<const iovec*>(1), 1, 0),
              -1);
    EXPECT_EQ(errno, EFAULT);
}

TEST(ProcessVmTest, EmptyRemoteIteratorSkipsPidLookup) {
    unsigned char byte = 0;
    iovec local = {};
    local.iov_base = &byte;
    local.iov_len = 1;
    iovec remote = {};
    remote.iov_base = reinterpret_cast<void*>(1);
    remote.iov_len = 0;

    errno = 0;
    EXPECT_EQ(syscall(SYS_process_vm_readv, 0, &local, 1, &remote, 1, 0), 0);
}

TEST(ProcessVmTest, ReadvUsesTheSharedIovecCursor) {
    const unsigned char source[] = {0x11, 0x22, 0x33, 0x44};
    unsigned char first[1] = {};
    unsigned char second[3] = {};
    iovec local[3] = {};
    local[0].iov_base = first;
    local[0].iov_len = sizeof(first);
    local[1].iov_base = first;
    local[1].iov_len = 0;
    local[2].iov_base = second;
    local[2].iov_len = sizeof(second);
    iovec remote[2] = {};
    remote[0].iov_base = const_cast<unsigned char*>(source);
    remote[0].iov_len = 2;
    remote[1].iov_base = const_cast<unsigned char*>(source + 2);
    remote[1].iov_len = 2;

    ASSERT_EQ(syscall(SYS_process_vm_readv, getpid(), local, 3, remote, 2, 0),
              static_cast<ssize_t>(sizeof(source)))
        << "errno=" << errno;
    EXPECT_EQ(first[0], source[0]);
    EXPECT_EQ(std::memcmp(second, source + 1, sizeof(second)), 0);
}

TEST(ProcessVmTest, LocalAndRemotePageHolesReturnExactProgress) {
    constexpr size_t kPageSize = 4096;
    constexpr size_t kOffset = 37;
    constexpr size_t kExpected = kPageSize - kOffset;
    unsigned char* guarded = static_cast<unsigned char*>(
        mmap(nullptr, 3 * kPageSize, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(guarded, MAP_FAILED);
    ASSERT_EQ(munmap(guarded + kPageSize, kPageSize), 0);
    std::memset(guarded, 0x4d, kPageSize);

    std::vector<unsigned char> valid_source(kPageSize, 0xa6);
    std::vector<unsigned char> valid_target(kPageSize, 0);
    iovec local = {};
    iovec remote = {};

    // A remote hole is consumed page-by-page by access_remote_vm().
    local = {valid_target.data(), valid_target.size()};
    remote = {guarded + kOffset, kPageSize};
    ASSERT_EQ(syscall(SYS_process_vm_readv, getpid(), &local, 1, &remote, 1, 0),
              static_cast<ssize_t>(kExpected));
    EXPECT_EQ(std::memcmp(valid_target.data(), guarded + kOffset, kExpected), 0);

    local = {valid_source.data(), valid_source.size()};
    remote = {guarded + kOffset, kPageSize};
    ASSERT_EQ(syscall(SYS_process_vm_writev, getpid(), &local, 1, &remote, 1, 0),
              static_cast<ssize_t>(kExpected));
    EXPECT_EQ(std::memcmp(guarded + kOffset, valid_source.data(), kExpected), 0);

    // The local user-copy side must retain the bytes completed before the
    // following page faults, matching Linux iov_iter progress semantics.
    std::memset(guarded, 0, kPageSize);
    local = {guarded + kOffset, kPageSize};
    remote = {valid_source.data(), valid_source.size()};
    ASSERT_EQ(syscall(SYS_process_vm_readv, getpid(), &local, 1, &remote, 1, 0),
              static_cast<ssize_t>(kExpected));
    EXPECT_EQ(std::memcmp(guarded + kOffset, valid_source.data(), kExpected), 0);

    std::memset(guarded, 0x7c, kPageSize);
    std::fill(valid_target.begin(), valid_target.end(), 0);
    local = {guarded + kOffset, kPageSize};
    remote = {valid_target.data(), valid_target.size()};
    ASSERT_EQ(syscall(SYS_process_vm_writev, getpid(), &local, 1, &remote, 1, 0),
              static_cast<ssize_t>(kExpected));
    EXPECT_EQ(std::memcmp(valid_target.data(), guarded + kOffset, kExpected), 0);

    // With no progress at all, either side's first-byte hole is EFAULT.
    unsigned char byte = 0;
    local = {&byte, 1};
    remote = {guarded + kPageSize, 1};
    errno = 0;
    EXPECT_EQ(syscall(SYS_process_vm_readv, getpid(), &local, 1, &remote, 1, 0), -1);
    EXPECT_EQ(errno, EFAULT);
    errno = 0;
    EXPECT_EQ(syscall(SYS_process_vm_writev, getpid(), &local, 1, &remote, 1, 0), -1);
    EXPECT_EQ(errno, EFAULT);

    local = {guarded + kPageSize, 1};
    remote = {&byte, 1};
    errno = 0;
    EXPECT_EQ(syscall(SYS_process_vm_readv, getpid(), &local, 1, &remote, 1, 0), -1);
    EXPECT_EQ(errno, EFAULT);
    errno = 0;
    EXPECT_EQ(syscall(SYS_process_vm_writev, getpid(), &local, 1, &remote, 1, 0), -1);
    EXPECT_EQ(errno, EFAULT);

    EXPECT_EQ(munmap(guarded, kPageSize), 0);
    EXPECT_EQ(munmap(guarded + 2 * kPageSize, kPageSize), 0);
}

TEST(ProcessVmTest, MaximumIovecCountKeepsOneMonotonicCursor) {
    constexpr size_t kIovCount = 1024;
    constexpr size_t kPayload = kIovCount / 2;
    std::vector<unsigned char> source(kPayload);
    std::vector<unsigned char> destination(kPayload, 0);
    std::vector<iovec> local(kIovCount);
    std::vector<iovec> remote(kIovCount);
    for (size_t index = 0; index < kPayload; ++index) {
        source[index] = static_cast<unsigned char>((index * 37 + 11) & 0xff);
        local[2 * index] = {destination.data() + index, 1};
        remote[2 * index] = {source.data() + index, 1};
        // Invalid bases on empty elements prove the cursor skips them without
        // touching the described range.
        local[2 * index + 1] = {reinterpret_cast<void*>(1), 0};
        remote[2 * index + 1] = {reinterpret_cast<void*>(1), 0};
    }

    ASSERT_EQ(syscall(SYS_process_vm_readv, getpid(), local.data(), local.size(),
                      remote.data(), remote.size(), 0),
              static_cast<ssize_t>(kPayload));
    EXPECT_EQ(destination, source);

    std::fill(destination.begin(), destination.end(), 0);
    for (size_t index = 0; index < kPayload; ++index) {
        local[2 * index].iov_base = source.data() + index;
        remote[2 * index].iov_base = destination.data() + index;
    }
    ASSERT_EQ(syscall(SYS_process_vm_writev, getpid(), local.data(), local.size(),
                      remote.data(), remote.size(), 0),
              static_cast<ssize_t>(kPayload));
    EXPECT_EQ(destination, source);
}

TEST(ProcessVmWritevTest, WritableAnonymousMappingIsUpdated) {
    void* mapping = mmap(nullptr, 4096, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(mapping, MAP_FAILED);
    const char value[] = "dragon";
    ASSERT_EQ(write_self(mapping, value, sizeof(value)),
              static_cast<ssize_t>(sizeof(value))) << "errno=" << errno;
    EXPECT_EQ(std::memcmp(mapping, value, sizeof(value)), 0);
    munmap(mapping, 4096);
}

TEST(ProcessVmWritevTest, ReadOnlyMappingReturnsEfault) {
    void* mapping = mmap(nullptr, 4096, PROT_READ,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(mapping, MAP_FAILED);
    const unsigned char value = 0x5a;
    errno = 0;
    EXPECT_EQ(write_self(mapping, &value, sizeof(value)), -1);
    EXPECT_EQ(errno, EFAULT);
    munmap(mapping, 4096);
}

TEST(ProcessVmWritevTest, PrivateFileWriteUsesCow) {
    char path[] = "/tmp/process_vm_cow_XXXXXX";
    const int fd = mkstemp(path);
    ASSERT_GE(fd, 0);
    ASSERT_EQ(ftruncate(fd, 4096), 0);
    const unsigned char original = 0x31;
    ASSERT_EQ(pwrite(fd, &original, 1, 0), 1);

    void* mapping = mmap(nullptr, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE,
                         fd, 0);
    ASSERT_NE(mapping, MAP_FAILED);
    const unsigned char replacement = 0x7b;
    ASSERT_EQ(write_self(mapping, &replacement, 1), 1) << "errno=" << errno;
    EXPECT_EQ(*static_cast<unsigned char*>(mapping), replacement);

    unsigned char backing = 0;
    ASSERT_EQ(pread(fd, &backing, 1, 0), 1);
    EXPECT_EQ(backing, original);

    munmap(mapping, 4096);
    close(fd);
    unlink(path);
}

TEST(ProcessVmWritevTest, SharedFileWriteIsDirtyAndPartialCountIsExact) {
    char path[] = "/tmp/process_vm_shared_XXXXXX";
    const int fd = mkstemp(path);
    ASSERT_GE(fd, 0);
    ASSERT_EQ(ftruncate(fd, 4096), 0);

    void* mapping = mmap(nullptr, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd,
                         0);
    ASSERT_NE(mapping, MAP_FAILED);
    const unsigned char values[] = {0x41, 0x42};
    iovec local = {};
    local.iov_base = const_cast<unsigned char*>(values);
    local.iov_len = sizeof(values);
    iovec remote[2] = {};
    remote[0].iov_base = mapping;
    remote[0].iov_len = 1;
    remote[1].iov_base = reinterpret_cast<void*>(1);
    remote[1].iov_len = 1;

    EXPECT_EQ(syscall(SYS_process_vm_writev, getpid(), &local, 1, remote, 2,
                      0),
              1)
        << "the invalid second range must not hide the completed first byte";
    EXPECT_EQ(msync(mapping, 4096, MS_SYNC), 0);
    unsigned char backing = 0;
    ASSERT_EQ(pread(fd, &backing, 1, 0), 1);
    EXPECT_EQ(backing, values[0]);

    munmap(mapping, 4096);
    close(fd);
    unlink(path);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
