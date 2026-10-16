#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include <algorithm>
#include <string>

namespace {

constexpr const char* kSelftestPath = "/sys/kernel/debug/mm/xattr_syscall_selftest";

class XattrSyscallSelftest : public ::testing::Test {
  protected:
    void SetUp() override {
        struct stat info = {};
        if (stat(kSelftestPath, &info) < 0) {
            const int error = errno;
            if (error == ENOENT) {
                GTEST_SKIP() << "build the kernel with KERNEL_FEATURES=mm_selftest";
            }
            FAIL() << "stat(" << kSelftestPath << "): " << strerror(error);
        }
    }
};

bool ReadReport(int fd, size_t chunk_size, std::string* report) {
    report->clear();
    char buffer[256];
    for (;;) {
        const ssize_t count = read(fd, buffer, std::min(chunk_size, sizeof(buffer)));
        if (count == 0) {
            return true;
        }
        if (count < 0) {
            if (errno == EINTR) {
                continue;
            }
            return false;
        }
        report->append(buffer, static_cast<size_t>(count));
    }
}

}  // namespace

TEST_F(XattrSyscallSelftest, ChecksPassAndSnapshotIsStableAcrossPartialReads) {
    // Reopening exercises complete allocation/release transactions repeatedly.
    for (int round = 0; round < 3; ++round) {
        SCOPED_TRACE(round);
        const int fd = open(kSelftestPath, O_RDONLY | O_CLOEXEC);
        ASSERT_GE(fd, 0) << strerror(errno);
        std::string partial;
        const bool partial_ok = ReadReport(fd, 7, &partial);
        const off_t rewind = lseek(fd, 0, SEEK_SET);
        std::string complete;
        const bool complete_ok = rewind == 0 && ReadReport(fd, 256, &complete);
        const int closed = close(fd);
        ASSERT_TRUE(partial_ok) << strerror(errno);
        ASSERT_EQ(0, rewind) << strerror(errno);
        ASSERT_TRUE(complete_ok) << strerror(errno);
        EXPECT_EQ(0, closed) << strerror(errno);
        EXPECT_EQ(partial, complete);
        EXPECT_NE(std::string::npos, complete.find("status=ok\n")) << complete;
        EXPECT_NE(std::string::npos, complete.find("summary_fail=0\n")) << complete;
        for (const char* check : {"input_snapshot=ok\n",
                                  "output_bounds=ok\n",
                                  "errors_release=ok\n",
                                  "charge_admission=ok\n",
                                  "fragmented_staging=ok\n"}) {
            EXPECT_NE(std::string::npos, complete.find(check)) << complete;
        }
    }
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
