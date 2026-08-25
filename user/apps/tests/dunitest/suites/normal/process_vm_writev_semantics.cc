#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <cerrno>
#include <cstdlib>
#include <cstring>

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
