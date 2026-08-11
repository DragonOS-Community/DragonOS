#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
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

bool wait_for_zombie(pid_t child) {
    siginfo_t info = {};
    int result = -1;
    do {
        result = waitid(P_PID, child, &info, WEXITED | WNOWAIT);
    } while (result < 0 && errno == EINTR);
    return result == 0 && info.si_pid == child;
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

struct NamespaceThreadResult {
    const char* ns_name = nullptr;
    pid_t tid = -1;
    int open_errno = 0;
    bool opened = false;
    bool link_read = false;
};

void* open_thread_namespace(void* opaque) {
    auto* result = static_cast<NamespaceThreadResult*>(opaque);
    result->tid = static_cast<pid_t>(syscall(SYS_gettid));

    char path[128] = {};
    snprintf(path, sizeof(path), "/proc/%d/task/%d/ns/%s", getpid(), result->tid,
             result->ns_name);

    const int fd = open(path, O_RDONLY);
    result->open_errno = errno;
    if (fd >= 0) {
        result->opened = true;
        close(fd);
    }

    char target[128] = {};
    result->link_read = readlink(path, target, sizeof(target)) > 0;
    return nullptr;
}

bool run_namespace_thread(NamespaceThreadResult* result) {
    pthread_t thread = {};
    if (pthread_create(&thread, nullptr, open_thread_namespace, result) != 0) {
        return false;
    }
    return pthread_join(thread, nullptr) == 0;
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

TEST(ProcPidReuseCache, ZombieFdEntriesDisappearWithEnoent) {
    int release_fd = -1;
    const pid_t child = fork_waiting_child(&release_fd);
    ASSERT_GT(child, 0);

    char fd_path[64] = {};
    snprintf(fd_path, sizeof(fd_path), "/proc/%d/fd/0", child);
    char link_target[64] = {};
    EXPECT_GE(readlink(fd_path, link_target, sizeof(link_target)), 0);

    char fdinfo_path[64] = {};
    snprintf(fdinfo_path, sizeof(fdinfo_path), "/proc/%d/fdinfo/0", child);
    const int live_fdinfo = open(fdinfo_path, O_RDONLY);
    EXPECT_GE(live_fdinfo, 0);

    const char byte = 'x';
    EXPECT_EQ(1, write(release_fd, &byte, 1));
    close(release_fd);

    const bool became_zombie = wait_for_zombie(child);
    if (became_zombie) {
        errno = 0;
        EXPECT_EQ(-1, readlink(fd_path, link_target, sizeof(link_target)));
        EXPECT_EQ(ENOENT, errno);

        errno = 0;
        const int fdinfo = open(fdinfo_path, O_RDONLY);
        EXPECT_EQ(-1, fdinfo);
        EXPECT_EQ(ENOENT, errno);
        if (fdinfo >= 0) {
            close(fdinfo);
        }

        if (live_fdinfo >= 0) {
            char byte = {};
            errno = 0;
            EXPECT_EQ(-1, read(live_fdinfo, &byte, sizeof(byte)));
            EXPECT_EQ(ENOENT, errno);
        }
    }
    if (live_fdinfo >= 0) {
        close(live_fdinfo);
    }

    int status = 0;
    pid_t waited = -1;
    do {
        waited = waitpid(child, &status, 0);
    } while (waited < 0 && errno == EINTR);
    EXPECT_EQ(child, waited);
    ASSERT_TRUE(became_zombie) << "child did not become observable as a zombie";
}

TEST(ProcPidReuseCache, ReusedTidRefreshesTaskNamespaceDirectory) {
    NamespaceThreadResult first;
    first.ns_name = "ipc";
    ASSERT_TRUE(run_namespace_thread(&first));
    ASSERT_GT(first.tid, 0);
    ASSERT_TRUE(first.opened)
        << "initial namespace open failed: errno=" << first.open_errno << " ("
        << strerror(first.open_errno) << ")";
    ASSERT_TRUE(first.link_read);

    constexpr int kMaxReuseAttempts = 128;
    bool observed_reuse = false;
    for (int attempt = 0; attempt < kMaxReuseAttempts; ++attempt) {
        NamespaceThreadResult replacement;
        replacement.ns_name = "uts";
        ASSERT_TRUE(run_namespace_thread(&replacement));
        ASSERT_TRUE(replacement.opened)
            << "replacement namespace open failed: tid=" << replacement.tid
            << " errno=" << replacement.open_errno << " (" << strerror(replacement.open_errno)
            << ")";
        ASSERT_TRUE(replacement.link_read);

        if (replacement.tid == first.tid) {
            observed_reuse = true;
            break;
        }
    }

    if (!observed_reuse) {
        GTEST_SKIP() << "TID reuse was not observed within the fixed attempt budget";
    }
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
