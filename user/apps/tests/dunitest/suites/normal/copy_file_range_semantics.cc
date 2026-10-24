#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <gtest/gtest.h>
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <string>

namespace {
ssize_t Copy(int in, off_t* input, int out, off_t* output, size_t len) {
    return syscall(SYS_copy_file_range, in, input, out, output, len, 0);
}

class CopyFileRangeSemantics : public ::testing::Test {
  protected:
    void SetUp() override {
        alarm(30);
        ASSERT_EQ(0, unshare(CLONE_NEWNS)) << strerror(errno);
        ASSERT_EQ(0, mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr));
        char pattern[] = "/tmp/cfr-semantics-XXXXXX";
        char* directory = mkdtemp(pattern);
        ASSERT_NE(nullptr, directory);
        root = directory;
        a = root + "/a"; b = root + "/b"; bind = root + "/bind";
        for (const auto& path : {a, b, bind}) ASSERT_EQ(0, mkdir(path.c_str(), 0700));
        ASSERT_EQ(0, mount("tmpfs", a.c_str(), "tmpfs", 0, "size=1m"));
        mounted_a = true;
        ASSERT_EQ(0, mount("tmpfs", b.c_str(), "tmpfs", 0, "size=1m"));
        mounted_b = true;
        ASSERT_EQ(0, mount(a.c_str(), bind.c_str(), nullptr, MS_BIND, nullptr));
        mounted_bind = true;
        source = open((a + "/source").c_str(), O_CREAT | O_RDWR, 0600);
        ASSERT_GE(source, 0);
        ASSERT_EQ(16, write(source, "0123456789abcdef", 16));
        target = open((b + "/target").c_str(), O_CREAT | O_RDWR, 0600);
        ASSERT_GE(target, 0);
        ASSERT_EQ(6, write(target, "target", 6));
        ASSERT_EQ(2, lseek(source, 2, SEEK_SET));
        ASSERT_EQ(3, lseek(target, 3, SEEK_SET));
    }
    void TearDown() override {
        if (source >= 0) close(source);
        if (target >= 0) close(target);
        if (mounted_bind) EXPECT_EQ(0, umount(bind.c_str()));
        if (mounted_b) EXPECT_EQ(0, umount(b.c_str()));
        if (mounted_a) EXPECT_EQ(0, umount(a.c_str()));
        for (const auto& path : {bind, b, a}) if (!path.empty()) rmdir(path.c_str());
        if (!root.empty()) rmdir(root.c_str());
        alarm(0);
    }
    void Rejected(size_t len, bool explicit_offsets, off_t input_position = 2) {
        off_t input = input_position, output = 3;
        errno = 0;
        EXPECT_EQ(-1, Copy(source, explicit_offsets ? &input : nullptr,
                           target, explicit_offsets ? &output : nullptr, len));
        EXPECT_EQ(EXDEV, errno);
        EXPECT_EQ(input_position, input);
        EXPECT_EQ(3, output);
        EXPECT_EQ(2, lseek(source, 0, SEEK_CUR));
        EXPECT_EQ(3, lseek(target, 0, SEEK_CUR));
        char contents[7] = {};
        EXPECT_EQ(6, pread(target, contents, sizeof(contents), 0));
        EXPECT_STREQ("target", contents);
    }
    std::string root, a, b, bind;
    int source = -1, target = -1;
    bool mounted_a = false, mounted_b = false, mounted_bind = false;
};

TEST_F(CopyFileRangeSemantics, CrossFilesystemExplicitPreservesState) { Rejected(4, true); }
TEST_F(CopyFileRangeSemantics, CrossFilesystemImplicitPreservesState) { Rejected(4, false); }
TEST_F(CopyFileRangeSemantics, CrossFilesystemZeroLengthStillChecksIdentity) { Rejected(0, true); }
TEST_F(CopyFileRangeSemantics, CrossFilesystemPastEofStillChecksIdentity) { Rejected(4, true, 1000); }

TEST_F(CopyFileRangeSemantics, BindMountUsesSharedSuperblockAndClipsEof) {
    int same = open((bind + "/same").c_str(), O_CREAT | O_RDWR, 0600);
    ASSERT_GE(same, 0);
    off_t input = 12, output = 1;
    EXPECT_EQ(4, Copy(source, &input, same, &output, 20));
    EXPECT_EQ(16, input);
    EXPECT_EQ(5, output);
    EXPECT_EQ(2, lseek(source, 0, SEEK_CUR));
    EXPECT_EQ(0, lseek(same, 0, SEEK_CUR));
    char data[5] = {};
    EXPECT_EQ(4, pread(same, data, 4, 1));
    EXPECT_STREQ("cdef", data);
    close(same);
}

TEST_F(CopyFileRangeSemantics, SameFilesystemImplicitPositionsAndOverlap) {
    int same = open((a + "/same").c_str(), O_CREAT | O_RDWR, 0600);
    ASSERT_GE(same, 0);
    EXPECT_EQ(5, Copy(source, nullptr, same, nullptr, 5));
    EXPECT_EQ(7, lseek(source, 0, SEEK_CUR));
    EXPECT_EQ(5, lseek(same, 0, SEEK_CUR));
    char data[6] = {};
    EXPECT_EQ(5, pread(same, data, 5, 0));
    EXPECT_STREQ("23456", data);
    off_t input = 0, output = 1;
    errno = 0;
    EXPECT_EQ(-1, Copy(source, &input, source, &output, 4));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(0, input);
    EXPECT_EQ(1, output);
    close(same);
}

TEST_F(CopyFileRangeSemantics, ProcFileRejectsBeforeMetadataEof) {
    int proc = open("/proc/version", O_RDONLY);
    ASSERT_GE(proc, 0);
    off_t input = 0, output = 0;
    errno = 0;
    EXPECT_EQ(-1, Copy(proc, &input, target, &output, 65536));
    EXPECT_EQ(EXDEV, errno);
    EXPECT_EQ(0, input);
    EXPECT_EQ(0, output);
    close(proc);
}
}
int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
