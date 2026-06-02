#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <regex>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>
#include <vector>

#ifndef CLONE_NEWNS
#define CLONE_NEWNS 0x00020000
#endif

namespace {

struct ChildProcessGuard {
    pid_t pid = -1;
    int quit_fd = -1;
    int detail_fd = -1;

    ~ChildProcessGuard() {
        if (quit_fd >= 0) {
            char quit = 'Q';
            const ssize_t ignored = write(quit_fd, &quit, 1);
            (void)ignored;
            close(quit_fd);
        }

        if (detail_fd >= 0) {
            close(detail_fd);
        }

        if (pid > 0) {
            int status = 0;
            while (waitpid(pid, &status, 0) < 0 && errno == EINTR) {
            }
        }
    }
};

int ensure_dir(const char* path) {
    struct stat st = {};

    if (stat(path, &st) == 0) {
        return S_ISDIR(st.st_mode) ? 0 : -1;
    }

    return mkdir(path, 0755);
}

void best_effort_rmdir(const char* path) {
    if (rmdir(path) != 0 && errno != ENOENT && errno != ENOTEMPTY) {
        ADD_FAILURE() << "rmdir failed for " << path << ": errno=" << errno << " ("
                      << strerror(errno) << ")";
    }
}

std::string read_all_from_fd(int fd) {
    std::string out;
    char buf[512];
    ssize_t n = 0;

    while ((n = read(fd, buf, sizeof(buf))) > 0) {
        out.append(buf, static_cast<size_t>(n));
    }

    return out;
}

bool read_text_file(const char* path, std::string* out) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return false;
    }

    out->clear();
    char buf[1024];
    ssize_t n = 0;
    while ((n = read(fd, buf, sizeof(buf))) > 0) {
        out->append(buf, static_cast<size_t>(n));
    }

    const int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return n >= 0;
}

size_t count_nonempty_lines(const std::string& content) {
    size_t count = 0;
    size_t start = 0;

    while (start <= content.size()) {
        const size_t end = content.find('\n', start);
        const size_t line_end = end == std::string::npos ? content.size() : end;
        if (line_end > start) {
            ++count;
        }
        if (end == std::string::npos) {
            break;
        }
        start = end + 1;
    }

    return count;
}

size_t count_mountstats_entries(const std::string& content) {
    size_t count = 0;
    size_t start = 0;

    while (start <= content.size()) {
        const size_t end = content.find('\n', start);
        const size_t line_end = end == std::string::npos ? content.size() : end;
        if (line_end > start) {
            const std::string line = content.substr(start, line_end - start);
            if (line.rfind("device ", 0) == 0 || line.rfind("no device ", 0) == 0) {
                ++count;
            }
        }
        if (end == std::string::npos) {
            break;
        }
        start = end + 1;
    }

    return count;
}

void expect_contains(const char* path, const std::string& content, const char* needle) {
    EXPECT_NE(std::string::npos, content.find(needle)) << path << " missing substring\nneedle="
                                                       << needle << "\ncontent=" << content;
}

void expect_not_contains(const char* path, const std::string& content, const char* needle) {
    EXPECT_EQ(std::string::npos, content.find(needle))
        << path << " unexpectedly contains substring\nneedle=" << needle << "\ncontent="
        << content;
}

[[noreturn]] void child_fail(int detail_fd, const char* step) {
    dprintf(detail_fd,
            "%s: errno=%d (%s)",
            step,
            errno,
            errno == 0 ? "no error information" : strerror(errno));
    _exit(1);
}

bool can_use_mount_namespaces() {
    if (geteuid() == 0) {
        return true;
    }

    const pid_t probe = fork();
    if (probe < 0) {
        return false;
    }

    if (probe == 0) {
        _exit(unshare(CLONE_NEWNS) == 0 ? 0 : 1);
    }

    int status = 0;
    if (waitpid(probe, &status, 0) != probe) {
        return false;
    }

    return WIFEXITED(status) && WEXITSTATUS(status) == 0;
}

}  // namespace

TEST(ProcMountExports, ProcMountsSymlinkTarget) {
    char target[256] = {};
    const ssize_t len = readlink("/proc/mounts", target, sizeof(target) - 1);
    ASSERT_GE(len, 0) << "readlink /proc/mounts failed: errno=" << errno << " ("
                      << strerror(errno) << ")";
    target[len] = '\0';
    EXPECT_STREQ(target, "self/mounts");
}

TEST(ProcMountExports, ProcMountsMatchesSelf) {
    std::string proc_mounts;
    std::string self_mounts;

    ASSERT_TRUE(read_text_file("/proc/mounts", &proc_mounts))
        << "read /proc/mounts failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_TRUE(read_text_file("/proc/self/mounts", &self_mounts))
        << "read /proc/self/mounts failed: errno=" << errno << " (" << strerror(errno) << ")";

    EXPECT_EQ(proc_mounts, self_mounts);
}

TEST(ProcMountExports, SelfMountExportLineCountsMatch) {
    std::string mounts;
    std::string mountinfo;
    std::string mountstats;

    ASSERT_TRUE(read_text_file("/proc/self/mounts", &mounts))
        << "read /proc/self/mounts failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_TRUE(read_text_file("/proc/self/mountinfo", &mountinfo))
        << "read /proc/self/mountinfo failed: errno=" << errno << " (" << strerror(errno)
        << ")";
    ASSERT_TRUE(read_text_file("/proc/self/mountstats", &mountstats))
        << "read /proc/self/mountstats failed: errno=" << errno << " (" << strerror(errno)
        << ")";

    EXPECT_GT(count_nonempty_lines(mounts), 0U);
    EXPECT_EQ(count_nonempty_lines(mounts), count_nonempty_lines(mountinfo));
    EXPECT_EQ(count_nonempty_lines(mounts), count_mountstats_entries(mountstats));
}

TEST(ProcMountExports, SelfMountExportOwnershipAndModes) {
    struct stat mounts = {};
    struct stat mountinfo = {};
    struct stat mountstats = {};

    ASSERT_EQ(0, stat("/proc/self/mounts", &mounts))
        << "stat /proc/self/mounts failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_EQ(0, stat("/proc/self/mountinfo", &mountinfo))
        << "stat /proc/self/mountinfo failed: errno=" << errno << " (" << strerror(errno)
        << ")";
    ASSERT_EQ(0, stat("/proc/self/mountstats", &mountstats))
        << "stat /proc/self/mountstats failed: errno=" << errno << " (" << strerror(errno)
        << ")";

    EXPECT_EQ(static_cast<uid_t>(geteuid()), mounts.st_uid);
    EXPECT_EQ(static_cast<gid_t>(getegid()), mounts.st_gid);
    EXPECT_EQ(static_cast<uid_t>(geteuid()), mountinfo.st_uid);
    EXPECT_EQ(static_cast<gid_t>(getegid()), mountinfo.st_gid);
    EXPECT_EQ(static_cast<uid_t>(geteuid()), mountstats.st_uid);
    EXPECT_EQ(static_cast<gid_t>(getegid()), mountstats.st_gid);

    EXPECT_EQ(0444U, static_cast<unsigned>(mounts.st_mode & 0777));
    EXPECT_EQ(0444U, static_cast<unsigned>(mountinfo.st_mode & 0777));
    EXPECT_EQ(0400U, static_cast<unsigned>(mountstats.st_mode & 0777));
}

TEST(ProcMountExports, NonRootCanReadOwnMountstats) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root to drop credentials";
    }

    int detail_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(detail_pipe)) << "pipe detail failed: errno=" << errno << " ("
                                    << strerror(errno) << ")";

    const pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        close(detail_pipe[0]);

        if (setgid(1000) != 0) {
            child_fail(detail_pipe[1], "setgid(1000)");
        }
        if (setuid(1000) != 0) {
            child_fail(detail_pipe[1], "setuid(1000)");
        }
        if (prctl(PR_SET_DUMPABLE, 1) != 0) {
            child_fail(detail_pipe[1], "prctl(PR_SET_DUMPABLE)");
        }

        struct stat mountstats = {};
        if (stat("/proc/self/mountstats", &mountstats) != 0) {
            child_fail(detail_pipe[1], "stat(/proc/self/mountstats)");
        }
        if (mountstats.st_uid != geteuid() || mountstats.st_gid != getegid()) {
            dprintf(detail_pipe[1],
                    "mountstats owner mismatch: st_uid=%u st_gid=%u euid=%u egid=%u",
                    static_cast<unsigned>(mountstats.st_uid),
                    static_cast<unsigned>(mountstats.st_gid),
                    static_cast<unsigned>(geteuid()),
                    static_cast<unsigned>(getegid()));
            _exit(1);
        }
        if ((mountstats.st_mode & 0777) != 0400) {
            dprintf(detail_pipe[1],
                    "mountstats mode mismatch: mode=%o",
                    static_cast<unsigned>(mountstats.st_mode & 0777));
            _exit(1);
        }

        std::string content;
        if (!read_text_file("/proc/self/mountstats", &content)) {
            child_fail(detail_pipe[1], "read(/proc/self/mountstats)");
        }
        if (content.empty()) {
            dprintf(detail_pipe[1], "mountstats content is empty");
            _exit(1);
        }

        close(detail_pipe[1]);
        _exit(0);
    }

    close(detail_pipe[1]);

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << "waitpid failed: errno=" << errno << " ("
                                                 << strerror(errno) << ")";

    const std::string detail = read_all_from_fd(detail_pipe[0]);
    close(detail_pipe[0]);

    ASSERT_TRUE(WIFEXITED(status)) << "child terminated abnormally, status=0x" << std::hex
                                   << status;
    EXPECT_EQ(0, WEXITSTATUS(status)) << detail;
}

TEST(ProcMountExports, SelfMountstatsLineFormat) {
    std::string mountstats;

    ASSERT_TRUE(read_text_file("/proc/self/mountstats", &mountstats))
        << "read /proc/self/mountstats failed: errno=" << errno << " (" << strerror(errno)
        << ")";

    const std::regex line_re(
        R"(^(device \S+|no device) mounted on \S+ with fstype \S+( .*)?$)",
        std::regex::ECMAScript);

    size_t start = 0;
    while (start <= mountstats.size()) {
        const size_t end = mountstats.find('\n', start);
        const size_t line_end = end == std::string::npos ? mountstats.size() : end;
        if (line_end > start) {
            const std::string line = mountstats.substr(start, line_end - start);
            EXPECT_TRUE(std::regex_match(line, line_re)) << "bad mountstats line: " << line;
        }
        if (end == std::string::npos) {
            break;
        }
        start = end + 1;
    }
}

TEST(ProcMountExports, UsesTargetTaskRootAndMountNamespace) {
    if (!can_use_mount_namespaces()) {
        GTEST_SKIP() << "requires CAP_SYS_ADMIN or unprivileged mount namespaces";
    }

    char base[256] = {};
    char rootfs[256] = {};
    char inside_name[64] = {};
    char inside[256] = {};
    char outside[256] = {};
    char proc_mounts_path[64] = {};
    char proc_mountinfo_path[64] = {};
    char proc_mountstats_path[64] = {};
    int ready_pipe[2] = {-1, -1};
    int quit_pipe[2] = {-1, -1};
    int detail_pipe[2] = {-1, -1};
    std::string proc_mounts;
    std::string proc_mountinfo;
    std::string proc_mountstats;
    std::string self_mounts;
    std::string self_mountinfo;
    std::string self_mountstats;
    char inside_mounts_needle[96] = {};
    char inside_mountinfo_needle[96] = {};
    char inside_mountstats_needle[96] = {};

    ASSERT_EQ(0, ensure_dir("/tmp")) << "mkdir /tmp failed: errno=" << errno << " ("
                                      << strerror(errno) << ")";

    snprintf(base, sizeof(base), "/tmp/proc_mount_exports_%d", getpid());
    snprintf(rootfs, sizeof(rootfs), "%s/rootfs", base);
    snprintf(inside_name, sizeof(inside_name), "inside_%d", getpid());
    snprintf(inside, sizeof(inside), "%s/%s", rootfs, inside_name);
    snprintf(outside, sizeof(outside), "%s/outside", base);
    snprintf(inside_mounts_needle, sizeof(inside_mounts_needle), " /%s ramfs ", inside_name);
    snprintf(inside_mountinfo_needle, sizeof(inside_mountinfo_needle), " /%s ", inside_name);
    snprintf(inside_mountstats_needle, sizeof(inside_mountstats_needle),
             " mounted on /%s with fstype ramfs", inside_name);

    ASSERT_EQ(0, ensure_dir(base)) << "mkdir base failed: errno=" << errno << " ("
                                   << strerror(errno) << ")";
    ASSERT_EQ(0, ensure_dir(rootfs)) << "mkdir rootfs failed: errno=" << errno << " ("
                                     << strerror(errno) << ")";
    ASSERT_EQ(0, ensure_dir(outside)) << "mkdir outside failed: errno=" << errno << " ("
                                      << strerror(errno) << ")";

    ASSERT_EQ(0, pipe(ready_pipe)) << "pipe ready failed: errno=" << errno << " ("
                                   << strerror(errno) << ")";
    ASSERT_EQ(0, pipe(quit_pipe)) << "pipe quit failed: errno=" << errno << " ("
                                  << strerror(errno) << ")";
    ASSERT_EQ(0, pipe(detail_pipe)) << "pipe detail failed: errno=" << errno << " ("
                                    << strerror(errno) << ")";

    const pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        close(ready_pipe[0]);
        close(quit_pipe[1]);
        close(detail_pipe[0]);

        if (unshare(CLONE_NEWNS) != 0) {
            child_fail(detail_pipe[1], "unshare(CLONE_NEWNS)");
        }
        if (mount("", "/", nullptr, MS_REC | MS_PRIVATE, nullptr) != 0) {
            child_fail(detail_pipe[1], "mount(/, MS_PRIVATE)");
        }
        if (mount("", rootfs, "ramfs", 0, nullptr) != 0) {
            child_fail(detail_pipe[1], "mount(rootfs, ramfs)");
        }
        if (ensure_dir(inside) != 0) {
            child_fail(detail_pipe[1], "mkdir(inside)");
        }
        if (mount("", inside, "ramfs", 0, nullptr) != 0) {
            child_fail(detail_pipe[1], "mount(inside, ramfs)");
        }
        if (mount("", outside, "ramfs", 0, nullptr) != 0) {
            child_fail(detail_pipe[1], "mount(outside, ramfs)");
        }
        if (chdir(rootfs) != 0) {
            child_fail(detail_pipe[1], "chdir(rootfs)");
        }
        if (chroot(rootfs) != 0) {
            child_fail(detail_pipe[1], "chroot(rootfs)");
        }
        if (chdir("/") != 0) {
            child_fail(detail_pipe[1], "chdir(/)");
        }

        const char ready = 'R';
        if (write(ready_pipe[1], &ready, 1) != 1) {
            child_fail(detail_pipe[1], "notify parent ready");
        }

        char quit = 0;
        if (read(quit_pipe[0], &quit, 1) != 1) {
            child_fail(detail_pipe[1], "wait parent quit");
        }

        close(ready_pipe[1]);
        close(quit_pipe[0]);
        close(detail_pipe[1]);
        _exit(0);
    }

    close(ready_pipe[1]);
    close(quit_pipe[0]);
    close(detail_pipe[1]);

    ChildProcessGuard guard;
    guard.pid = child;
    guard.quit_fd = quit_pipe[1];
    guard.detail_fd = detail_pipe[0];

    char ready = 0;
    ASSERT_EQ(1, read(ready_pipe[0], &ready, 1)) << "read child ready failed: errno=" << errno
                                                 << " (" << strerror(errno) << ")";
    EXPECT_EQ('R', ready);
    close(ready_pipe[0]);

    snprintf(proc_mounts_path, sizeof(proc_mounts_path), "/proc/%d/mounts", child);
    ASSERT_TRUE(read_text_file(proc_mounts_path, &proc_mounts))
        << "read " << proc_mounts_path << " failed: errno=" << errno << " (" << strerror(errno)
        << ")";
    expect_contains(proc_mounts_path, proc_mounts, " / ramfs ");
    expect_contains(proc_mounts_path, proc_mounts, inside_mounts_needle);
    expect_not_contains(proc_mounts_path, proc_mounts, "/outside");

    snprintf(proc_mountinfo_path, sizeof(proc_mountinfo_path), "/proc/%d/mountinfo", child);
    ASSERT_TRUE(read_text_file(proc_mountinfo_path, &proc_mountinfo))
        << "read " << proc_mountinfo_path << " failed: errno=" << errno << " ("
        << strerror(errno) << ")";
    expect_contains(proc_mountinfo_path, proc_mountinfo, inside_mountinfo_needle);
    expect_not_contains(proc_mountinfo_path, proc_mountinfo, outside);

    snprintf(proc_mountstats_path, sizeof(proc_mountstats_path), "/proc/%d/mountstats", child);
    ASSERT_TRUE(read_text_file(proc_mountstats_path, &proc_mountstats))
        << "read " << proc_mountstats_path << " failed: errno=" << errno << " ("
        << strerror(errno) << ")";
    expect_contains(proc_mountstats_path, proc_mountstats, inside_mountstats_needle);
    expect_not_contains(proc_mountstats_path, proc_mountstats, outside);
    EXPECT_EQ(count_nonempty_lines(proc_mounts), count_nonempty_lines(proc_mountinfo));
    EXPECT_EQ(count_nonempty_lines(proc_mounts), count_mountstats_entries(proc_mountstats));

    ASSERT_TRUE(read_text_file("/proc/self/mounts", &self_mounts))
        << "read /proc/self/mounts failed: errno=" << errno << " (" << strerror(errno) << ")";
    expect_not_contains("/proc/self/mounts", self_mounts, inside_mountinfo_needle);
    expect_not_contains("/proc/self/mounts", self_mounts, outside);

    ASSERT_TRUE(read_text_file("/proc/self/mountinfo", &self_mountinfo))
        << "read /proc/self/mountinfo failed: errno=" << errno << " (" << strerror(errno)
        << ")";
    expect_not_contains("/proc/self/mountinfo", self_mountinfo, inside_mountinfo_needle);
    expect_not_contains("/proc/self/mountinfo", self_mountinfo, outside);

    ASSERT_TRUE(read_text_file("/proc/self/mountstats", &self_mountstats))
        << "read /proc/self/mountstats failed: errno=" << errno << " (" << strerror(errno)
        << ")";
    expect_not_contains("/proc/self/mountstats", self_mountstats, inside_mountstats_needle);
    expect_not_contains("/proc/self/mountstats", self_mountstats, outside);

    char quit = 'Q';
    ASSERT_EQ(1, write(guard.quit_fd, &quit, 1)) << "write child quit failed: errno=" << errno
                                                 << " (" << strerror(errno) << ")";
    close(guard.quit_fd);
    guard.quit_fd = -1;

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << "waitpid failed: errno=" << errno << " ("
                                                 << strerror(errno) << ")";

    const std::string detail = read_all_from_fd(guard.detail_fd);
    close(guard.detail_fd);
    guard.detail_fd = -1;

    ASSERT_TRUE(WIFEXITED(status)) << "child terminated abnormally, status=0x" << std::hex
                                   << status;
    EXPECT_EQ(0, WEXITSTATUS(status)) << detail;

    guard.pid = -1;

    best_effort_rmdir(inside);
    best_effort_rmdir(rootfs);
    best_effort_rmdir(outside);
    best_effort_rmdir(base);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
