#include <gtest/gtest.h>

#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <string>
#include <unistd.h>

TEST(MountForcedReadonly, ExistingWriterAndLateRemountCannotBypassProtection) {
    int fd = open("/sys/kernel/debug/vfs/forced_readonly_selftest", O_RDONLY | O_CLOEXEC);
    if (fd < 0 && errno == ENOENT) GTEST_SKIP() << "requires mm_selftest kernel";
    ASSERT_GE(fd, 0) << strerror(errno);
    std::string report;
    char chunk[256];
    for (;;) {
        ssize_t count = read(fd, chunk, sizeof(chunk));
        if (count < 0 && errno == EINTR) continue;
        if (count < 0) {
            int error = errno;
            close(fd);
            FAIL() << strerror(error);
        }
        if (count == 0) break;
        report.append(chunk, count);
    }
    ASSERT_EQ(0, close(fd));
    for (const char* key : {"status", "held_writer", "new_writer", "late_commit", "flags_setter", "new_rw_rejected"}) {
        EXPECT_NE(std::string::npos, report.find(std::string(key) + "=ok\n")) << report;
    }
}

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
