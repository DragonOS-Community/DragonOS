#include <gtest/gtest.h>

#include <errno.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/uio.h>
#include <sys/wait.h>

#include <string>
#include <vector>

#include "../fuse/fuse_gtest_common.h"

namespace {
constexpr size_t kReadCapacity = 1024 * 1024 + FUSE_TEST_BUF_SIZE;

struct QueryResult {
    int result;
    int error;
};

class FuseReadFault : public ::testing::Test {
  protected:
    void SetUp() override {
        if (geteuid() != 0) GTEST_SKIP() << "FUSE mount requires root";
        daemon_fd = open("/dev/fuse", O_RDWR | O_NONBLOCK | O_CLOEXEC);
        if (daemon_fd < 0 && (errno == ENOENT || errno == ENODEV || errno == EPERM ||
                              errno == EACCES)) {
            GTEST_SKIP() << "/dev/fuse unavailable: " << strerror(errno);
        }
        ASSERT_GE(daemon_fd, 0) << strerror(errno);
        char directory[] = "/tmp/dunitest_fuse_fault_XXXXXX";
        char* created = mkdtemp(directory);
        ASSERT_NE(nullptr, created) << strerror(errno);
        root = created;
        char options[256];
        snprintf(options, sizeof(options), "fd=%d,rootmode=040755,user_id=%u,group_id=%u",
                 daemon_fd, getuid(), getgid());
        int result = mount("none", root.c_str(), "fuse", 0, options);
        if (result < 0 && (errno == EPERM || errno == EACCES)) {
            GTEST_SKIP() << "FUSE mounting unavailable: " << strerror(errno);
        }
        ASSERT_EQ(0, result) << strerror(errno);
        mounted = true;
        ASSERT_EQ(0, fuseg_do_init_handshake_basic(daemon_fd)) << strerror(errno);
    }

    void TearDown() override {
        // Closing the daemon aborts outstanding FUSE requests before child
        // cleanup. Never wait indefinitely for a broken copy-fault lifecycle.
        if (daemon_fd >= 0) close(daemon_fd);
        if (mounted) {
            EXPECT_EQ(0, umount2(root.c_str(), MNT_DETACH)) << strerror(errno);
        }
        if (child > 0) {
            kill(child, SIGKILL);
            bool reaped = false;
            for (int attempt = 0; attempt < 100; ++attempt) {
                int status;
                const pid_t result = waitpid(child, &status, WNOHANG);
                if (result == child || (result < 0 && errno == ECHILD)) {
                    reaped = true;
                    break;
                }
                usleep(10000);
            }
            EXPECT_TRUE(reaped) << "FUSE query child did not exit after abort";
        }
        if (result_fd >= 0) close(result_fd);
        if (!root.empty()) {
            EXPECT_EQ(0, rmdir(root.c_str())) << strerror(errno);
        }
    }

    void StartQuery(const char* name) {
        int report[2];
        ASSERT_EQ(0, pipe2(report, O_NONBLOCK | O_CLOEXEC));
        const std::string path = root + "/" + name;
        child = fork();
        if (child < 0) {
            close(report[0]);
            close(report[1]);
            FAIL() << "fork: " << strerror(errno);
        }
        if (child == 0) {
            close(report[0]);
            close(daemon_fd);
            struct stat info = {};
            QueryResult result;
            result.result = lstat(path.c_str(), &info);
            result.error = result.result < 0 ? errno : 0;
            const ssize_t written = write(report[1], &result, sizeof(result));
            close(report[1]);
            _exit(written == sizeof(result) ? 0 : 1);
        }
        close(report[1]);
        result_fd = report[0];
    }

    void ExpectQueryError(int expected) {
        ASSERT_EQ(0, fuseg_wait_readable(result_fd, 3000))
            << "FUSE query remained blocked: " << strerror(errno);
        QueryResult result = {};
        ASSERT_EQ(static_cast<ssize_t>(sizeof(result)), read(result_fd, &result, sizeof(result)));
        EXPECT_EQ(-1, result.result);
        EXPECT_EQ(expected, result.error);
        close(result_fd);
        result_fd = -1;
        for (int attempt = 0; attempt < 100; ++attempt) {
            int status;
            const pid_t reaped = waitpid(child, &status, WNOHANG);
            if (reaped == child) {
                child = -1;
                ASSERT_TRUE(WIFEXITED(status));
                EXPECT_EQ(0, WEXITSTATUS(status));
                return;
            }
            ASSERT_GE(reaped, 0) << strerror(errno);
            usleep(10000);
        }
        FAIL() << "FUSE query reported a result but did not exit";
    }

    void ServeMissingQuery() {
        std::vector<unsigned char> request(kReadCapacity);
        // A root GETATTR may precede LOOKUP depending on cached attributes.
        // Bound both the number of requests and each device wait.
        for (int attempt = 0; attempt < 8; ++attempt) {
            ASSERT_EQ(0, fuseg_wait_readable(daemon_fd, 3000)) << strerror(errno);
            const ssize_t count = read(daemon_fd, request.data(), request.size());
            ASSERT_GE(count, static_cast<ssize_t>(sizeof(fuse_in_header))) << strerror(errno);
            fuse_in_header header = {};
            memcpy(&header, request.data(), sizeof(header));
            ASSERT_EQ(static_cast<uint32_t>(count), header.len);
            if (header.opcode == FUSE_LOOKUP) {
                ASSERT_EQ(0, fuse_write_reply(daemon_fd, header.unique, -ENOENT, nullptr, 0));
                return;
            }
            if (header.opcode == FUSE_GETATTR && header.nodeid == 1) {
                fuse_attr_out attributes = {};
                attributes.attr.ino = 1;
                attributes.attr.mode = S_IFDIR | 0755;
                attributes.attr.nlink = 2;
                attributes.attr.uid = getuid();
                attributes.attr.gid = getgid();
                attributes.attr.blksize = 4096;
                ASSERT_EQ(0, fuse_write_reply(daemon_fd, header.unique, 0, &attributes,
                                             sizeof(attributes)));
            } else {
                FAIL() << "unexpected FUSE opcode " << header.opcode;
            }
        }
        FAIL() << "FUSE query never reached LOOKUP";
    }

    void CheckFault(bool vectored) {
        ASSERT_NO_FATAL_FAILURE(StartQuery("faulted"));
        ASSERT_EQ(0, fuseg_wait_readable(daemon_fd, 3000)) << strerror(errno);
        void* inaccessible = mmap(nullptr, kReadCapacity, PROT_NONE,
                                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        ASSERT_NE(MAP_FAILED, inaccessible);
        unsigned char prefix[16] = {};
        iovec vectors[] = {{prefix, sizeof(prefix)},
                           {inaccessible, kReadCapacity - sizeof(prefix)}};
        errno = 0;
        const ssize_t count = vectored ? readv(daemon_fd, vectors, 2)
                                       : read(daemon_fd, inaccessible, kReadCapacity);
        const int error = errno;
        EXPECT_EQ(0, munmap(inaccessible, kReadCapacity));
        ASSERT_EQ(-1, count);
        ASSERT_EQ(EFAULT, error);
        ASSERT_NO_FATAL_FAILURE(ExpectQueryError(EIO));

        // A copy fault retires only the failed request, not the connection.
        ASSERT_NO_FATAL_FAILURE(StartQuery("after_fault"));
        ASSERT_NO_FATAL_FAILURE(ServeMissingQuery());
        ASSERT_NO_FATAL_FAILURE(ExpectQueryError(ENOENT));
    }

    int daemon_fd = -1;
    int result_fd = -1;
    pid_t child = -1;
    bool mounted = false;
    std::string root;
};

TEST_F(FuseReadFault, ScalarCopyFaultCompletesRequestAndConnectionRemainsUsable) {
    CheckFault(false);
}

TEST_F(FuseReadFault, VectoredCopyFaultCompletesRequestAndConnectionRemainsUsable) {
    CheckFault(true);
}
}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
