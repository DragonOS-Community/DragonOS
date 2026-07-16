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

bool write_text_file(const char* path, const char* content) {
    int fd = open(path, O_WRONLY | O_TRUNC);
    if (fd < 0) {
        return false;
    }

    const char* p = content;
    size_t left = strlen(content);
    while (left > 0) {
        ssize_t n = write(fd, p, left);
        if (n < 0) {
            int saved_errno = errno;
            close(fd);
            errno = saved_errno;
            return false;
        }
        p += n;
        left -= static_cast<size_t>(n);
    }

    int saved_errno = errno;
    bool ok = close(fd) == 0;
    if (!ok) {
        saved_errno = errno;
    }
    errno = saved_errno;
    return ok;
}

bool path_exists(const char* path) {
    struct stat st = {};
    return stat(path, &st) == 0;
}

void expect_file(int detail_fd, const char* path) {
    struct stat st = {};
    if (stat(path, &st) != 0) {
        child_fail(detail_fd, path);
    }
    if (!S_ISREG(st.st_mode)) {
        errno = EINVAL;
        child_fail(detail_fd, path);
    }
}

void expect_missing(int detail_fd, const char* path) {
    struct stat st = {};
    errno = 0;
    if (stat(path, &st) == 0) {
        errno = EEXIST;
        child_fail(detail_fd, path);
    }
    if (errno != ENOENT) {
        child_fail(detail_fd, path);
    }
}

void expect_text_has(int detail_fd, const char* path, const char* needle) {
    std::string content;
    if (!read_text_file(path, &content)) {
        child_fail(detail_fd, path);
    }
    if (content.find(needle) == std::string::npos) {
        errno = EINVAL;
        child_fail(detail_fd, path);
    }
}

void expect_text_not_has(int detail_fd, const char* path, const char* needle) {
    std::string content;
    if (!read_text_file(path, &content)) {
        child_fail(detail_fd, path);
    }
    if (content.find(needle) != std::string::npos) {
        errno = EINVAL;
        child_fail(detail_fd, path);
    }
}

void expect_write_errno(int detail_fd, const char* path, const char* content, int expected_errno) {
    errno = 0;
    if (write_text_file(path, content)) {
        errno = 0;
        child_fail(detail_fd, path);
    }
    if (errno != expected_errno) {
        child_fail(detail_fd, path);
    }
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

void expect_root_p1_visibility(int detail_fd) {
    expect_file(detail_fd, "/sys/fs/cgroup/cgroup.procs");
    expect_file(detail_fd, "/sys/fs/cgroup/cgroup.controllers");
    expect_file(detail_fd, "/sys/fs/cgroup/cgroup.subtree_control");
    expect_file(detail_fd, "/sys/fs/cgroup/cpu.stat");
    expect_file(detail_fd, "/sys/fs/cgroup/memory.stat");

    expect_missing(detail_fd, "/sys/fs/cgroup/cgroup.events");
    expect_missing(detail_fd, "/sys/fs/cgroup/cgroup.type");
    expect_missing(detail_fd, "/sys/fs/cgroup/cgroup.freeze");
    expect_missing(detail_fd, "/sys/fs/cgroup/cpu.weight");
    expect_missing(detail_fd, "/sys/fs/cgroup/cpu.max");
    expect_missing(detail_fd, "/sys/fs/cgroup/memory.current");
    expect_missing(detail_fd, "/sys/fs/cgroup/memory.max");
    expect_missing(detail_fd, "/sys/fs/cgroup/pids.current");
    expect_missing(detail_fd, "/sys/fs/cgroup/pids.max");
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

TEST(SysfsCgroup2Mount, AppendWritesKeepUsingPseudoFileEof) {
    run_in_child("AppendWritesKeepUsingPseudoFileEof", [](int detail_fd) {
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

        int fd = open("/sys/fs/cgroup/cgroup.procs", O_WRONLY | O_APPEND);
        if (fd < 0) {
            child_fail(detail_fd, "open cgroup.procs O_APPEND");
        }
        static constexpr char kCurrentTask[] = "0\n";
        for (int i = 0; i < 2; ++i) {
            if (write(fd, kCurrentTask, sizeof(kCurrentTask) - 1) !=
                static_cast<ssize_t>(sizeof(kCurrentTask) - 1)) {
                close(fd);
                child_fail(detail_fd, "write cgroup.procs O_APPEND");
            }
        }
        close(fd);
    });
}

TEST(SysfsCgroup2Mount, Cgroup2P1ControllerFilesFollowSubtreeControl) {
    run_in_child("Cgroup2P1ControllerFilesFollowSubtreeControl", [](int detail_fd) {
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

        expect_text_has(detail_fd, "/sys/fs/cgroup/cgroup.controllers", "cpu");
        expect_text_has(detail_fd, "/sys/fs/cgroup/cgroup.controllers", "memory");
        expect_text_has(detail_fd, "/sys/fs/cgroup/cgroup.controllers", "pids");
        expect_text_not_has(detail_fd, "/sys/fs/cgroup/cgroup.controllers", "cpuacct");
        expect_text_not_has(detail_fd, "/sys/fs/cgroup/cgroup.controllers", "freezer");
        expect_text_not_has(detail_fd, "/sys/fs/cgroup/cgroup.controllers", "net_cls");
        expect_text_not_has(detail_fd, "/sys/fs/cgroup/cgroup.controllers", "net_prio");
        expect_text_not_has(detail_fd, "/sys/fs/cgroup/cgroup.controllers", "oom");
        expect_root_p1_visibility(detail_fd);

        if (!write_text_file("/sys/fs/cgroup/cgroup.subtree_control",
                             "+cpu +memory +pids")) {
            child_fail(detail_fd, "enable root subtree controllers");
        }
        expect_root_p1_visibility(detail_fd);

        std::string parent = "/sys/fs/cgroup/dunit_cgv2_p1_";
        parent += std::to_string(getpid());
        std::string child = parent + "/child";

        if (mkdir(parent.c_str(), 0755) != 0) {
            child_fail(detail_fd, parent.c_str());
        }
        if (mkdir(child.c_str(), 0755) != 0) {
            child_fail(detail_fd, child.c_str());
        }

        std::string child_cpu_max = child + "/cpu.max";
        if (path_exists(child_cpu_max.c_str())) {
            errno = EEXIST;
            child_fail(detail_fd, "child cpu.max before parent enable");
        }
        std::string child_cpu_stat = child + "/cpu.stat";
        expect_file(detail_fd, child_cpu_stat.c_str());

        std::string parent_subtree = parent + "/cgroup.subtree_control";
        if (!write_text_file(parent_subtree.c_str(), "+cpu +memory +pids")) {
            child_fail(detail_fd, "enable parent subtree controllers");
        }

        const char* files[] = {
            "/cpu.stat",
            "/cpu.weight",
            "/cpu.max",
            "/memory.current",
            "/memory.peak",
            "/memory.min",
            "/memory.low",
            "/memory.high",
            "/memory.max",
            "/memory.events",
            "/memory.stat",
            "/memory.swap.current",
            "/memory.swap.peak",
            "/memory.swap.high",
            "/memory.swap.max",
            "/memory.swap.events",
            "/pids.current",
            "/pids.max",
            "/pids.events",
            "/cgroup.freeze",
        };
        for (const char* file : files) {
            std::string path = child + file;
            expect_file(detail_fd, path.c_str());
        }

        std::string cpu_weight = child + "/cpu.weight";
        if (!write_text_file(cpu_weight.c_str(), "200")) {
            child_fail(detail_fd, "write cpu.weight");
        }
        expect_text_has(detail_fd, cpu_weight.c_str(), "200\n");
        expect_write_errno(detail_fd, cpu_weight.c_str(), "0", ERANGE);

        if (!write_text_file(child_cpu_max.c_str(), "50000 100000")) {
            child_fail(detail_fd, "write cpu.max quota");
        }
        expect_text_has(detail_fd, child_cpu_max.c_str(), "50000 100000\n");
        if (!write_text_file(child_cpu_max.c_str(), "max 100000")) {
            child_fail(detail_fd, "write cpu.max max");
        }
        expect_text_has(detail_fd, child_cpu_max.c_str(), "max 100000\n");

        std::string memory_max = child + "/memory.max";
        std::string memory_high = child + "/memory.high";
        std::string memory_low = child + "/memory.low";
        std::string memory_min = child + "/memory.min";
        std::string swap_max = child + "/memory.swap.max";
        if (!write_text_file(memory_max.c_str(), "max") ||
            !write_text_file(memory_high.c_str(), "4096") ||
            !write_text_file(memory_low.c_str(), "4096") ||
            !write_text_file(memory_min.c_str(), "0") ||
            !write_text_file(swap_max.c_str(), "max")) {
            child_fail(detail_fd, "write memory limits");
        }
        expect_text_has(detail_fd, memory_high.c_str(), "4096\n");
        expect_text_has(detail_fd, memory_low.c_str(), "4096\n");

        std::string freeze = child + "/cgroup.freeze";
        if (!write_text_file(freeze.c_str(), "1")) {
            child_fail(detail_fd, "write cgroup.freeze 1");
        }
        expect_text_has(detail_fd, freeze.c_str(), "1\n");
        std::string events = child + "/cgroup.events";
        expect_text_has(detail_fd, events.c_str(), "frozen 0\n");
        if (!write_text_file(freeze.c_str(), "0")) {
            child_fail(detail_fd, "write cgroup.freeze 0");
        }
        expect_write_errno(detail_fd, freeze.c_str(), "2", ERANGE);

        std::string memory_events = child + "/memory.events";
        expect_text_has(detail_fd, memory_events.c_str(), "oom_group_kill 0\n");
        std::string swap_events = child + "/memory.swap.events";
        expect_text_has(detail_fd, swap_events.c_str(), "fail 0\n");
        std::string cpu_stat = child + "/cpu.stat";
        expect_text_has(detail_fd, cpu_stat.c_str(), "usage_usec 0\n");

        if (!write_text_file(parent_subtree.c_str(), "-cpu")) {
            child_fail(detail_fd, "disable parent cpu controller");
        }
        if (path_exists(child_cpu_max.c_str())) {
            errno = EEXIST;
            child_fail(detail_fd, "child cpu.max after parent disable");
        }
        expect_file(detail_fd, child_cpu_stat.c_str());

        rmdir(child.c_str());
        rmdir(parent.c_str());
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
