#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include <fstream>
#include <sstream>
#include <string>

#ifndef CLONE_NEWNS
#define CLONE_NEWNS 0x00020000
#endif

#ifndef MS_REC
#define MS_REC 16384
#endif

#ifndef MS_RELATIME
#define MS_RELATIME (1 << 21)
#endif

namespace {

[[noreturn]] void child_fail(int detail_fd, const char* step) {
    dprintf(detail_fd,
            "%s: errno=%d (%s)",
            step,
            errno,
            errno == 0 ? "no error information" : strerror(errno));
    _exit(1);
}

template <typename Fn>
void run_in_child(const char* case_name, Fn fn) {
    int detail_pipe[2];
    ASSERT_EQ(0, pipe(detail_pipe)) << strerror(errno);

    pid_t pid = fork();
    ASSERT_GE(pid, 0) << strerror(errno);

    if (pid == 0) {
        close(detail_pipe[0]);
        fn(detail_pipe[1]);
        close(detail_pipe[1]);
        _exit(0);
    }

    close(detail_pipe[1]);

    std::string detail;
    char buf[256];
    ssize_t n = 0;
    while ((n = read(detail_pipe[0], buf, sizeof(buf))) > 0) {
        detail.append(buf, static_cast<size_t>(n));
    }
    close(detail_pipe[0]);

    int status = 0;
    while (waitpid(pid, &status, 0) < 0 && errno == EINTR) {
    }

    ASSERT_TRUE(WIFEXITED(status)) << case_name << " child terminated abnormally";
    ASSERT_EQ(0, WEXITSTATUS(status)) << case_name << " child failed: " << detail;
}

void expect_dir(int detail_fd, const char* path) {
    struct stat st = {};
    if (stat(path, &st) != 0) {
        child_fail(detail_fd, path);
    }
    if (!S_ISDIR(st.st_mode)) {
        errno = ENOTDIR;
        child_fail(detail_fd, path);
    }
}

void make_private_mount_namespace(int detail_fd) {
    if (unshare(CLONE_NEWNS) != 0) {
        child_fail(detail_fd, "unshare(CLONE_NEWNS)");
    }

    if (mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr) != 0) {
        child_fail(detail_fd, "mount MS_REC|MS_PRIVATE /");
    }
}

void mount_sysfs_on_sys(int detail_fd) {
    if (mount("sysfs", "/sys", "sysfs", MS_NOSUID | MS_NODEV | MS_NOEXEC, nullptr) != 0) {
        child_fail(detail_fd, "mount sysfs /sys");
    }
}

void ensure_dir_recursive(int detail_fd, const char* path) {
    std::string current;
    const char* p = path;

    if (*p == '/') {
        current = "/";
        ++p;
    }

    while (*p != '\0') {
        const char* start = p;
        while (*p != '\0' && *p != '/') {
            ++p;
        }

        if (p > start) {
            if (current.size() > 1) {
                current.push_back('/');
            }
            current.append(start, static_cast<size_t>(p - start));

            struct stat st = {};
            if (stat(current.c_str(), &st) == 0) {
                if (!S_ISDIR(st.st_mode)) {
                    errno = ENOTDIR;
                    child_fail(detail_fd, current.c_str());
                }
            } else if (errno == ENOENT) {
                if (mkdir(current.c_str(), 0755) != 0 && errno != EEXIST) {
                    child_fail(detail_fd, current.c_str());
                }
            } else {
                child_fail(detail_fd, current.c_str());
            }
        }

        while (*p == '/') {
            ++p;
        }
    }
}

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

    int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return n >= 0;
}

bool mounts_has_exact_entry(const char* mountpoint, const char* fstype) {
    std::ifstream mounts("/proc/self/mounts");
    std::string line;

    while (std::getline(mounts, line)) {
        std::istringstream fields(line);
        std::string source;
        std::string target;
        std::string type;
        if (!(fields >> source >> target >> type)) {
            continue;
        }
        if (target == mountpoint && type == fstype) {
            return true;
        }
    }

    return false;
}

}  // namespace

TEST(SysfsCgroup2Mount, SysfsRemountPreservesKernelMountPoint) {
    run_in_child("SysfsRemountPreservesKernelMountPoint", [](int detail_fd) {
        expect_dir(detail_fd, "/sys");
        expect_dir(detail_fd, "/sys/fs");
        expect_dir(detail_fd, "/sys/fs/cgroup");

        make_private_mount_namespace(detail_fd);
        mount_sysfs_on_sys(detail_fd);

        expect_dir(detail_fd, "/sys/fs");
        expect_dir(detail_fd, "/sys/fs/cgroup");
    });
}

TEST(SysfsCgroup2Mount, CubeAgentUnifiedCgroupSequenceSucceeds) {
    run_in_child("CubeAgentUnifiedCgroupSequenceSucceeds", [](int detail_fd) {
        make_private_mount_namespace(detail_fd);
        mount_sysfs_on_sys(detail_fd);

        ensure_dir_recursive(detail_fd, "/sys/fs/cgroup");

        if (mount("cgroup2",
                  "/sys/fs/cgroup",
                  "cgroup2",
                  MS_NOSUID | MS_NODEV | MS_NOEXEC | MS_RELATIME,
                  "nsdelegate") != 0) {
            child_fail(detail_fd, "mount cgroup2 /sys/fs/cgroup");
        }

        std::string content;
        if (!read_text_file("/sys/fs/cgroup/cgroup.procs", &content)) {
            child_fail(detail_fd, "read cgroup.procs");
        }
        if (!read_text_file("/sys/fs/cgroup/cgroup.controllers", &content)) {
            child_fail(detail_fd, "read cgroup.controllers");
        }
        if (!mounts_has_exact_entry("/sys/fs/cgroup", "cgroup2")) {
            errno = ENOENT;
            child_fail(detail_fd, "find cgroup2 in /proc/self/mounts");
        }
    });
}

TEST(SysfsCgroup2Mount, SysfsRejectsArbitraryUserMkdir) {
    run_in_child("SysfsRejectsArbitraryUserMkdir", [](int detail_fd) {
        make_private_mount_namespace(detail_fd);
        mount_sysfs_on_sys(detail_fd);

        const char* path = "/sys/dragonos_user_created_should_fail";
        if (mkdir(path, 0755) == 0) {
            errno = 0;
            child_fail(detail_fd, "mkdir unexpectedly succeeded in sysfs");
        }
    });
}

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
