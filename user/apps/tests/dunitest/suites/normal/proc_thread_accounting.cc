#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
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
    char buf[256];
    ssize_t n = 0;
    while ((n = read(fd, buf, sizeof(buf))) > 0) {
        out->append(buf, static_cast<size_t>(n));
    }

    const int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return n >= 0;
}

void write_text_or_die(const char* path, const char* text) {
    const int fd = open(path, O_WRONLY);
    if (fd < 0) {
        dprintf(STDERR_FILENO,
                "open(%s) failed: errno=%d (%s)\n",
                path,
                errno,
                strerror(errno));
        _exit(1);
    }

    const size_t len = strlen(text);
    const ssize_t n = write(fd, text, len);
    const int saved_errno = errno;
    close(fd);
    if (n != static_cast<ssize_t>(len)) {
        dprintf(STDERR_FILENO,
                "write(%s) failed: n=%zd errno=%d (%s)\n",
                path,
                n,
                saved_errno,
                strerror(saved_errno));
        _exit(1);
    }
}

unsigned long read_loadavg_thread_total() {
    std::string loadavg;
    if (!read_text_file("/proc/loadavg", &loadavg)) {
        ADD_FAILURE() << "read /proc/loadavg failed: errno=" << errno << " (" << strerror(errno)
                      << ")";
        return 0;
    }

    unsigned long running = 0;
    unsigned long total = 0;
    if (sscanf(loadavg.c_str(), "%*s %*s %*s %lu/%lu", &running, &total) != 2) {
        ADD_FAILURE() << "failed to parse /proc/loadavg: " << loadavg;
        return 0;
    }

    return total;
}

}  // namespace

TEST(ProcThreadAccounting, RejectedForkDoesNotDecrementVisibleThreads) {
    const unsigned long before_total = read_loadavg_thread_total();
    ASSERT_GT(before_total, 0UL);

    char cgroup_dir[128] = {};
    char cgroup_procs[160] = {};
    char pids_max[160] = {};
    snprintf(cgroup_dir, sizeof(cgroup_dir), "/sys/fs/cgroup/proc_thread_accounting_%d", getpid());
    snprintf(cgroup_procs, sizeof(cgroup_procs), "%s/cgroup.procs", cgroup_dir);
    snprintf(pids_max, sizeof(pids_max), "%s/pids.max", cgroup_dir);

    ASSERT_EQ(0, mkdir(cgroup_dir, 0755)) << "mkdir cgroup failed: errno=" << errno << " ("
                                          << strerror(errno) << ")";

    const pid_t child = fork();
    ASSERT_GE(child, 0) << "fork child failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        write_text_or_die(cgroup_procs, "0\n");
        write_text_or_die(pids_max, "1\n");

        for (int i = 0; i < 64; ++i) {
            errno = 0;
            const pid_t failed = fork();
            if (failed == 0) {
                _exit(2);
            }
            if (failed > 0) {
                int status = 0;
                waitpid(failed, &status, 0);
                dprintf(STDERR_FILENO, "fork unexpectedly succeeded under pids.max=1\n");
                _exit(1);
            }
            if (errno != EAGAIN) {
                dprintf(STDERR_FILENO,
                        "fork failed with unexpected errno=%d (%s)\n",
                        errno,
                        strerror(errno));
                _exit(1);
            }
        }

        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << "waitpid failed: errno=" << errno << " ("
                                                << strerror(errno) << ")";
    ASSERT_TRUE(WIFEXITED(status)) << "child did not exit normally, status=" << status;
    ASSERT_EQ(0, WEXITSTATUS(status));

    const unsigned long after_total = read_loadavg_thread_total();
    EXPECT_LT(after_total, 100000UL)
        << "rejected fork polluted visible thread accounting: before=" << before_total
        << " after=" << after_total;
    EXPECT_LE(after_total, before_total + 16)
        << "rejected fork should not inflate visible thread total: before=" << before_total
        << " after=" << after_total;

    if (rmdir(cgroup_dir) != 0 && errno != ENOENT) {
        ADD_FAILURE() << "rmdir cgroup failed: errno=" << errno << " (" << strerror(errno) << ")";
    }
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
