#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>

namespace {

bool read_text_file(const char* path, std::string* out) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return false;
    }

    out->clear();
    char buf[512];
    ssize_t n = 0;
    while ((n = read(fd, buf, sizeof(buf))) > 0) {
        out->append(buf, static_cast<size_t>(n));
    }

    const int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return n >= 0;
}

pid_t fork_waiting_child(int* release_fd) {
    int pipefd[2] = {-1, -1};
    if (pipe(pipefd) != 0) {
        return -1;
    }

    pid_t child = fork();
    if (child == 0) {
        close(pipefd[1]);
        char byte = 0;
        while (read(pipefd[0], &byte, 1) > 0) {
        }
        close(pipefd[0]);
        _exit(0);
    }

    close(pipefd[0]);
    if (child < 0) {
        close(pipefd[1]);
        return -1;
    }

    *release_fd = pipefd[1];
    return child;
}

void release_child(pid_t child, int release_fd) {
    if (release_fd >= 0) {
        close(release_fd);
    }

    int status = 0;
    while (waitpid(child, &status, 0) < 0 && errno == EINTR) {
    }
}

}  // namespace

TEST(ProcPidReuseCache, NumericProcDirRefreshesAfterPidReuse) {
    int first_release = -1;
    const pid_t first = fork_waiting_child(&first_release);
    ASSERT_GT(first, 0) << "fork first child failed: errno=" << errno << " (" << strerror(errno)
                        << ")";

    char status_path[64] = {};
    snprintf(status_path, sizeof(status_path), "/proc/%d/status", first);

    std::string status;
    ASSERT_TRUE(read_text_file(status_path, &status))
        << "initial read of " << status_path << " failed: errno=" << errno << " ("
        << strerror(errno) << ")";

    release_child(first, first_release);

    pid_t reused = -1;
    int reused_release = -1;
    for (int i = 0; i < 128; ++i) {
        int release_fd = -1;
        pid_t child = fork_waiting_child(&release_fd);
        ASSERT_GT(child, 0) << "fork replacement child failed: errno=" << errno << " ("
                            << strerror(errno) << ")";

        if (child == first) {
            reused = child;
            reused_release = release_fd;
            break;
        }

        release_child(child, release_fd);
    }

    ASSERT_EQ(first, reused) << "test requires PID reuse to exercise cached /proc/<pid> inode";

    status.clear();
    ASSERT_TRUE(read_text_file(status_path, &status))
        << "read after PID reuse should refresh stale /proc/<pid> cache: errno=" << errno << " ("
        << strerror(errno) << ")";

    char pid_line[32] = {};
    snprintf(pid_line, sizeof(pid_line), "Pid:\t%d", reused);
    EXPECT_NE(std::string::npos, status.find(pid_line)) << status_path << " content:\n" << status;

    release_child(reused, reused_release);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
