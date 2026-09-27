#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <linux/capability.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>
#include <vector>

#ifndef CLONE_FS
#define CLONE_FS 0x00000200
#endif

#ifndef CLONE_NEWUSER
#define CLONE_NEWUSER 0x10000000
#endif

#ifndef SYS_setns
#ifdef __NR_setns
#define SYS_setns __NR_setns
#endif
#endif

namespace {

constexpr size_t kCloneStackSize = 1 << 20;
constexpr int kChildWaitTimeoutSec = 5;

std::string errno_detail(const char* step, int err) {
    std::string detail(step);
    detail += ": errno=";
    detail += std::to_string(err);
    detail += " (";
    detail += strerror(err);
    detail += ")";
    return detail;
}

std::string expected_map_line(unsigned first, unsigned lower, unsigned count) {
    char buf[128] = {};
    snprintf(buf, sizeof(buf), "%10u %10u %10u\n", first, lower, count);
    return std::string(buf);
}

int write_text_file(const char* path, const std::string& content) {
    int fd = open(path, O_WRONLY);
    if (fd < 0) {
        return errno;
    }

    size_t written = 0;
    while (written < content.size()) {
        ssize_t n = write(fd, content.data() + written, content.size() - written);
        if (n < 0) {
            int err = errno;
            close(fd);
            return err;
        }
        written += static_cast<size_t>(n);
    }

    close(fd);
    return 0;
}

int read_text_file(const char* path, std::string* out) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return errno;
    }

    out->clear();
    char buf[128];
    for (;;) {
        ssize_t n = read(fd, buf, sizeof(buf));
        if (n < 0) {
            int err = errno;
            close(fd);
            return err;
        }
        if (n == 0) {
            break;
        }
        out->append(buf, static_cast<size_t>(n));
    }

    close(fd);
    return 0;
}

int read_link_text(const char* path, std::string* out) {
    char buf[256] = {};
    ssize_t n = readlink(path, buf, sizeof(buf) - 1);
    if (n < 0) {
        return errno;
    }

    buf[n] = '\0';
    *out = std::string(buf, static_cast<size_t>(n));
    return 0;
}

void send_detail(int fd, const std::string& detail) {
    if (fd < 0 || detail.empty()) {
        return;
    }

    size_t written = 0;
    while (written < detail.size()) {
        ssize_t n = write(fd, detail.data() + written, detail.size() - written);
        if (n <= 0) {
            return;
        }
        written += static_cast<size_t>(n);
    }
}

std::string read_pipe_detail(int fd) {
    std::string out;
    char buf[256];

    for (;;) {
        ssize_t n = read(fd, buf, sizeof(buf));
        if (n <= 0) {
            break;
        }
        out.append(buf, static_cast<size_t>(n));
    }

    return out;
}

bool wait_for_child_exit(pid_t child, int* status, int timeout_sec, bool kill_on_timeout,
                         std::string* detail) {
    constexpr useconds_t kPollIntervalUs = 100000;
    const int max_polls = timeout_sec * 1000000 / kPollIntervalUs;

    for (int i = 0; i < max_polls; ++i) {
        pid_t ret = waitpid(child, status, WNOHANG);
        if (ret == child) {
            return true;
        }
        if (ret < 0) {
            if (detail != nullptr) {
                *detail = errno_detail("waitpid", errno);
            }
            return false;
        }
        usleep(kPollIntervalUs);
    }

    if (kill_on_timeout) {
        kill(child, SIGKILL);
        waitpid(child, status, 0);
    }
    if (detail != nullptr) {
        *detail = "timed out waiting for child";
    }
    return false;
}

using UserNsRunner = int (*)(void*, std::string*);

struct CloneContext {
    UserNsRunner runner;
    void* arg;
    int status_fd;
};

struct FirstLevelArgs {
    unsigned uid;
    unsigned gid;
};

struct NestedLevelArgs {
    unsigned expected_visible_uid;
};

struct HeldClone {
    pid_t pid = -1;
    int release_fd = -1;
    std::vector<char> stack = std::vector<char>(kCloneStackSize);
};

struct HoldCloneContext {
    int ready_fd;
    int wait_fd;
};

int clone_runner_entry(void* opaque) {
    CloneContext* ctx = static_cast<CloneContext*>(opaque);
    std::string detail;
    int rc = ctx->runner(ctx->arg, &detail);
    send_detail(ctx->status_fd, detail);
    close(ctx->status_fd);
    return rc;
}

int hold_clone_entry(void* opaque) {
    HoldCloneContext* ctx = static_cast<HoldCloneContext*>(opaque);
    char ready = '1';
    if (write(ctx->ready_fd, &ready, 1) != 1) {
        close(ctx->ready_fd);
        close(ctx->wait_fd);
        return 1;
    }

    close(ctx->ready_fd);
    char byte = 0;
    while (read(ctx->wait_fd, &byte, 1) < 0) {
        if (errno != EINTR) {
            close(ctx->wait_fd);
            return 1;
        }
    }

    close(ctx->wait_fd);
    return 0;
}

int run_in_new_userns(UserNsRunner runner, void* arg, std::string* detail, int extra_flags = 0) {
    int pipefd[2] = {-1, -1};
    if (pipe(pipefd) != 0) {
        *detail = errno_detail("pipe", errno);
        return 1;
    }

    std::vector<char> stack(kCloneStackSize);
    CloneContext ctx = {
        .runner = runner,
        .arg = arg,
        .status_fd = pipefd[1],
    };

    pid_t child = clone(clone_runner_entry, stack.data() + stack.size(), CLONE_NEWUSER | extra_flags | SIGCHLD,
                        &ctx);
    if (child < 0) {
        int err = errno;
        close(pipefd[0]);
        close(pipefd[1]);
        *detail = errno_detail("clone(CLONE_NEWUSER)", err);
        return 1;
    }

    close(pipefd[1]);

    int status = 0;
    std::string wait_detail;
    if (!wait_for_child_exit(child, &status, kChildWaitTimeoutSec, true, &wait_detail)) {
        *detail = "clone child wait failed: " + wait_detail;
        std::string child_detail = read_pipe_detail(pipefd[0]);
        if (!child_detail.empty()) {
            *detail += "; child detail: " + child_detail;
        }
        close(pipefd[0]);
        return 1;
    }

    *detail = read_pipe_detail(pipefd[0]);
    close(pipefd[0]);

    if (!WIFEXITED(status)) {
        if (detail->empty()) {
            *detail = "child terminated abnormally";
        }
        return 1;
    }

    return WEXITSTATUS(status);
}

bool spawn_held_clone(int clone_flags, HeldClone* held, std::string* detail) {
    int ready_pipe[2] = {-1, -1};
    int release_pipe[2] = {-1, -1};
    if (pipe(ready_pipe) != 0) {
        *detail = errno_detail("pipe(ready)", errno);
        return false;
    }
    if (pipe(release_pipe) != 0) {
        int err = errno;
        close(ready_pipe[0]);
        close(ready_pipe[1]);
        *detail = errno_detail("pipe(release)", err);
        return false;
    }

    HoldCloneContext ctx = {
        .ready_fd = ready_pipe[1],
        .wait_fd = release_pipe[0],
    };

    pid_t child = clone(hold_clone_entry, held->stack.data() + held->stack.size(),
                        clone_flags | SIGCHLD, &ctx);
    if (child < 0) {
        int err = errno;
        close(ready_pipe[0]);
        close(ready_pipe[1]);
        close(release_pipe[0]);
        close(release_pipe[1]);
        *detail = errno_detail("clone(held child)", err);
        return false;
    }

    close(ready_pipe[1]);
    close(release_pipe[0]);

    char ready = 0;
    ssize_t n = read(ready_pipe[0], &ready, 1);
    close(ready_pipe[0]);
    if (n != 1) {
        int status = 0;
        std::string wait_detail;
        wait_for_child_exit(child, &status, kChildWaitTimeoutSec, true, &wait_detail);
        close(release_pipe[1]);
        *detail = "held child did not reach ready state";
        return false;
    }

    held->pid = child;
    held->release_fd = release_pipe[1];
    return true;
}

void cleanup_held_clone(HeldClone* held) {
    if (held->pid < 0) {
        return;
    }

    if (held->release_fd >= 0) {
        char byte = 'x';
        ssize_t ignored = write(held->release_fd, &byte, 1);
        (void)ignored;
        close(held->release_fd);
        held->release_fd = -1;
    }

    int status = 0;
    std::string detail;
    if (!wait_for_child_exit(held->pid, &status, kChildWaitTimeoutSec, true, &detail)) {
        // best effort cleanup; test process will report the original failure path
    }
    held->pid = -1;
}

int open_userns_fd(pid_t pid, std::string* detail) {
    char path[64] = {};
    snprintf(path, sizeof(path), "/proc/%d/ns/user", pid);
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        *detail = errno_detail("open target user ns fd", errno);
        return -1;
    }
    return fd;
}

int read_userns_link_for_pid(pid_t pid, std::string* out) {
    char path[64] = {};
    snprintf(path, sizeof(path), "/proc/%d/ns/user", pid);
    return read_link_text(path, out);
}

int run_second_level_userns(void* opaque, std::string* detail) {
    const NestedLevelArgs& args = *static_cast<const NestedLevelArgs*>(opaque);

    std::string inherited_policy;
    int err = read_text_file("/proc/self/setgroups", &inherited_policy);
    if (err != 0 || inherited_policy != "deny\n") {
        *detail = "nested user namespace did not inherit setgroups=deny";
        return 1;
    }

    err = write_text_file("/proc/self/uid_map", "0 0 1\n");
    if (err != 0) {
        *detail = errno_detail("write nested uid_map", err);
        return 1;
    }

    err = write_text_file("/proc/self/setgroups", "deny\n");
    if (err != 0) {
        *detail = errno_detail("write nested setgroups=deny", err);
        return 1;
    }

    err = write_text_file("/proc/self/gid_map", "0 0 1\n");
    if (err != 0) {
        *detail = errno_detail("write nested gid_map", err);
        return 1;
    }

    if (getuid() != 0 || geteuid() != 0 || getgid() != 0 || getegid() != 0) {
        *detail = "nested namespace IDs were not translated to local zero";
        return 1;
    }

    std::string content;
    err = read_text_file("/proc/self/uid_map", &content);
    if (err != 0) {
        *detail = errno_detail("read nested uid_map", err);
        return 1;
    }

    if (content != expected_map_line(0, args.expected_visible_uid, 1)) {
        *detail = "nested uid_map content mismatch: got='" + content + "' expected='"
            + expected_map_line(0, args.expected_visible_uid, 1) + "'";
        return 1;
    }

    return 0;
}

int run_first_level_userns(void* opaque, std::string* detail) {
    const FirstLevelArgs& args = *static_cast<const FirstLevelArgs*>(opaque);
    const std::string first_uid_map = "0 " + std::to_string(args.uid) + " 1\n";
    const std::string first_gid_map = "0 " + std::to_string(args.gid) + " 1\n";

    int err = write_text_file("/proc/self/uid_map", "4294967294 1000 2\n");
    if (err != EINVAL) {
        *detail = "uid_map accepted extent covering invalid UID";
        return 1;
    }
    err = write_text_file("/proc/self/uid_map", first_uid_map);
    if (err != 0) {
        *detail = errno_detail("write first uid_map", err);
        return 1;
    }

    err = write_text_file("/proc/self/gid_map", first_gid_map);
    if (err != EPERM) {
        *detail = "first gid_map before setgroups deny returned ";
        *detail += std::to_string(err);
        *detail += ", expected EPERM(";
        *detail += std::to_string(EPERM);
        *detail += ")";
        return 1;
    }

    err = write_text_file("/proc/self/setgroups", "deny\n");
    if (err != 0) {
        *detail = errno_detail("write first setgroups=deny", err);
        return 1;
    }

    err = write_text_file("/proc/self/gid_map", first_gid_map);
    if (err != 0) {
        *detail = errno_detail("write first gid_map after deny", err);
        return 1;
    }

    if (getuid() != 0 || geteuid() != 0 || getgid() != 0 || getegid() != 0) {
        *detail = "first namespace IDs were not translated to local zero";
        return 1;
    }
    uid_t ruid = 1, euid = 1, suid = 1;
    gid_t rgid = 1, egid = 1, sgid = 1;
    if (getresuid(&ruid, &euid, &suid) != 0 || ruid != 0 || euid != 0 || suid != 0 ||
        getresgid(&rgid, &egid, &sgid) != 0 || rgid != 0 || egid != 0 || sgid != 0) {
        *detail = "getresuid/getresgid returned global IDs";
        return 1;
    }
    // uid_t/gid_t syscall arguments are 32-bit even when passed in 64-bit registers.
    if (syscall(SYS_setuid, 1ULL << 32) != 0 || syscall(SYS_setgid, 1ULL << 32) != 0 ||
        getuid() != 0 || getgid() != 0) {
        *detail = "setuid/setgid did not truncate arguments to the ABI width";
        return 1;
    }
    errno = 0;
    if (setuid(1) != -1 || errno != EINVAL) {
        *detail = "setuid accepted an unmapped local ID";
        return 1;
    }
    errno = 0;
    if (setgid(1) != -1 || errno != EINVAL) {
        *detail = "setgid accepted an unmapped local ID";
        return 1;
    }

    char path[96] = {};
    snprintf(path, sizeof(path), "/tmp/dkc015-id-%d", getpid());
    int fd = open(path, O_CREAT | O_EXCL | O_RDWR, 0600);
    if (fd < 0) {
        *detail = errno_detail("create identity stat fixture", errno);
        return 1;
    }
    struct stat st = {};
    bool stat_ok = fstat(fd, &st) == 0 && st.st_uid == 0 && st.st_gid == 0;
    close(fd);
    unlink(path);
    if (!stat_ok) {
        *detail = "fstat did not translate global owner into namespace IDs";
        return 1;
    }

    std::string content;
    err = read_text_file("/proc/self/uid_map", &content);
    if (err != 0) {
        *detail = errno_detail("read first uid_map", err);
        return 1;
    }

    if (content != expected_map_line(0, args.uid, 1)) {
        *detail = "first uid_map content mismatch: got='" + content + "' expected='"
            + expected_map_line(0, args.uid, 1) + "'";
        return 1;
    }

    NestedLevelArgs nested = {
        .expected_visible_uid = 0,
    };
    std::string nested_detail;
    int rc = run_in_new_userns(run_second_level_userns, &nested, &nested_detail);
    if (rc != 0) {
        *detail = "nested userns failed: " + nested_detail;
        return 1;
    }

    return 0;
}

int run_rootless_nested_id_map_flow(std::string* detail) {
    unsigned uid = static_cast<unsigned>(geteuid());
    unsigned gid = static_cast<unsigned>(getegid());

    if (uid == 0) {
        if (setgid(1000) != 0) {
            *detail = errno_detail("setgid(1000)", errno);
            return 1;
        }
        if (setuid(1000) != 0) {
            *detail = errno_detail("setuid(1000)", errno);
            return 1;
        }
        uid = static_cast<unsigned>(geteuid());
        gid = static_cast<unsigned>(getegid());
    }

    FirstLevelArgs args = {
        .uid = uid,
        .gid = gid,
    };
    return run_in_new_userns(run_first_level_userns, &args, detail);
}

int reject_child_range_across_parent_extents(void*, std::string* detail) {
    int err = write_text_file("/proc/self/uid_map", "0 0 3\n");
    if (err != EPERM) {
        *detail = "nested map spanning parent extents returned " + std::to_string(err) +
                  ", expected EPERM";
        return 1;
    }
    return 0;
}

int run_discontinuous_parent_map_flow(std::string* detail) {
    if (geteuid() != 0) return 0;
    int ready[2] = {-1, -1};
    int release[2] = {-1, -1};
    if (pipe(ready) != 0 || pipe(release) != 0) {
        *detail = errno_detail("create map synchronization pipes", errno);
        return 1;
    }
    pid_t child = fork();
    if (child < 0) {
        *detail = errno_detail("fork map target", errno);
        return 1;
    }
    if (child == 0) {
        close(ready[0]);
        close(release[1]);
        if (unshare(CLONE_NEWUSER) != 0) _exit(2);
        char byte = 'r';
        if (write(ready[1], &byte, 1) != 1) _exit(3);
        if (read(release[0], &byte, 1) != 1) _exit(4);
        std::string nested_detail;
        int rc = run_in_new_userns(reject_child_range_across_parent_extents, nullptr,
                                   &nested_detail);
        _exit(rc == 0 ? 0 : 5);
    }
    close(ready[1]);
    close(release[0]);
    char byte = 0;
    bool child_ready = read(ready[0], &byte, 1) == 1;
    close(ready[0]);
    int uid_err = EIO, gid_err = EIO;
    if (child_ready) {
        char path[64] = {};
        snprintf(path, sizeof(path), "/proc/%d/uid_map", child);
        uid_err = write_text_file(path, "0 0 1\n1 1000 1\n2 2 1\n");
        snprintf(path, sizeof(path), "/proc/%d/gid_map", child);
        gid_err = write_text_file(path, "0 0 1\n");
    }
    byte = 'g';
    bool released = write(release[1], &byte, 1) == 1;
    close(release[1]);
    int status = 0;
    if (!wait_for_child_exit(child, &status, kChildWaitTimeoutSec, true, detail) ||
        !WIFEXITED(status) || WEXITSTATUS(status) != 0 || uid_err != 0 || gid_err != 0 ||
        !released) {
        *detail = "parent map write errors uid=" + std::to_string(uid_err) +
                  " gid=" + std::to_string(gid_err) +
                  " child_status=" + std::to_string(WIFEXITED(status) ? WEXITSTATUS(status) : -1);
        return 1;
    }
    return 0;
}

int run_dropped_map_writer_flow(std::string* detail) {
    if (geteuid() != 0) return 0;
    HeldClone held;
    if (!spawn_held_clone(CLONE_NEWUSER, &held, detail)) return 1;

    char path[64] = {};
    snprintf(path, sizeof(path), "/proc/%d/uid_map", held.pid);
    int fd = open(path, O_WRONLY);
    if (fd < 0) {
        *detail = errno_detail("open child uid_map", errno);
        cleanup_held_clone(&held);
        return 1;
    }

    pid_t writer = fork();
    if (writer < 0) {
        *detail = errno_detail("fork dropped map writer", errno);
        close(fd);
        cleanup_held_clone(&held);
        return 1;
    }
    constexpr char kWideMap[] = "0 0 1\n1 1 1\n";
    if (writer == 0) {
        if (setuid(1000) != 0) _exit(2);
        errno = 0;
        ssize_t written = write(fd, kWideMap, sizeof(kWideMap) - 1);
        _exit(written == -1 && errno == EPERM ? 0 : 3);
    }
    int status = 0;
    bool waited = wait_for_child_exit(writer, &status, kChildWaitTimeoutSec, true, detail);
    // A rejected write must leave the map empty and the original opener may
    // still install the same map with its own current CAP_SETUID.
    ssize_t written = waited ? write(fd, kWideMap, sizeof(kWideMap) - 1) : -1;
    close(fd);
    cleanup_held_clone(&held);
    if (!waited || !WIFEXITED(status) || WEXITSTATUS(status) != 0 ||
        written != static_cast<ssize_t>(sizeof(kWideMap) - 1)) {
        *detail = "a dropped writer changed uid_map through a pre-opened fd";
        return 1;
    }
    return 0;
}

int run_map_fd_after_target_exit_flow(std::string* detail) {
    if (geteuid() != 0) return 0;
    HeldClone held;
    if (!spawn_held_clone(CLONE_NEWUSER, &held, detail)) return 1;
    char path[64] = {};
    snprintf(path, sizeof(path), "/proc/%d/uid_map", held.pid);
    int uid_fd = open(path, O_RDWR);
    snprintf(path, sizeof(path), "/proc/%d/gid_map", held.pid);
    int gid_fd = open(path, O_RDWR);
    snprintf(path, sizeof(path), "/proc/%d/setgroups", held.pid);
    int setgroups_fd = open(path, O_RDWR);
    cleanup_held_clone(&held);
    if (uid_fd < 0 || gid_fd < 0 || setgroups_fd < 0) {
        *detail = "failed to open userns map/control files before target exit";
        if (uid_fd >= 0) close(uid_fd);
        if (gid_fd >= 0) close(gid_fd);
        if (setgroups_fd >= 0) close(setgroups_fd);
        return 1;
    }

    constexpr char kMap[] = "0 0 1\n";
    constexpr char kDeny[] = "deny\n";
    bool ok = write(uid_fd, kMap, sizeof(kMap) - 1) ==
                  static_cast<ssize_t>(sizeof(kMap) - 1) &&
              write(setgroups_fd, kDeny, sizeof(kDeny) - 1) ==
                  static_cast<ssize_t>(sizeof(kDeny) - 1) &&
              write(gid_fd, kMap, sizeof(kMap) - 1) ==
                  static_cast<ssize_t>(sizeof(kMap) - 1);
    char buf[64] = {};
    if (ok) {
        ok = lseek(uid_fd, 0, SEEK_SET) == 0 &&
             read(uid_fd, buf, sizeof(buf) - 1) > 0 &&
             std::string(buf) == expected_map_line(0, 0, 1);
    }
    close(uid_fd);
    close(gid_fd);
    close(setgroups_fd);
    if (!ok) {
        *detail = "an open map/control fd lost its user namespace after target exit";
        return 1;
    }
    return 0;
}

int run_setgroups_open_capability_flow(std::string* detail) {
    if (geteuid() != 0) return 0;
    // An ancestor with euid equal to a user namespace's owner has all
    // capabilities *in that namespace*, even after dropping CAP_SYS_ADMIN.
    // Create the target as uid 1000, then regain euid 0 in this test process.
    if (setresuid(0, 1000, 0) != 0) {
        *detail = errno_detail("drop euid before creating target namespace", errno);
        return 1;
    }
    HeldClone held;
    bool spawned = spawn_held_clone(CLONE_NEWUSER, &held, detail);
    if (setresuid(0, 0, 0) != 0) {
        *detail = errno_detail("restore root euid", errno);
        if (spawned) cleanup_held_clone(&held);
        return 1;
    }
    if (!spawned) return 1;
    char path[64] = {};
    snprintf(path, sizeof(path), "/proc/%d/setgroups", held.pid);
    int authorized_fd = open(path, O_WRONLY);
    if (authorized_fd < 0) {
        *detail = errno_detail("open setgroups with CAP_SYS_ADMIN", errno);
        cleanup_held_clone(&held);
        return 1;
    }
    pid_t writer = fork();
    if (writer < 0) {
        *detail = errno_detail("fork setgroups writer", errno);
        close(authorized_fd);
        cleanup_held_clone(&held);
        return 1;
    }
    if (writer == 0) {
        __user_cap_header_struct hdr = {.version = _LINUX_CAPABILITY_VERSION_3, .pid = 0};
        __user_cap_data_struct caps[2] = {};
        if (syscall(SYS_capget, &hdr, caps) != 0) _exit(2);
        caps[CAP_SYS_ADMIN / 32].effective &= ~(1u << (CAP_SYS_ADMIN % 32));
        if (syscall(SYS_capset, &hdr, caps) != 0) _exit(3);
        int read_fd = open(path, O_RDONLY);
        if (read_fd < 0) _exit(4);
        close(read_fd);
        errno = 0;
        int denied_fd = open(path, O_WRONLY);
        if (denied_fd >= 0) close(denied_fd);
        if (denied_fd != -1 || errno != EACCES) _exit(5);
        constexpr char kDeny[] = "deny\n";
        if (write(authorized_fd, kDeny, sizeof(kDeny) - 1) !=
            static_cast<ssize_t>(sizeof(kDeny) - 1)) _exit(6);
        _exit(0);
    }
    int status = 0;
    bool waited = wait_for_child_exit(writer, &status, kChildWaitTimeoutSec, true, detail);
    close(authorized_fd);
    cleanup_held_clone(&held);
    if (!waited || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        *detail = "setgroups open-time CAP_SYS_ADMIN check or pre-opened fd semantics failed; child=" +
                  std::to_string(WIFEXITED(status) ? WEXITSTATUS(status) : -1);
        return 1;
    }
    return 0;
}

int verify_clone_vm_userns_child(void*, std::string* detail) {
    std::string current_ns;
    std::string init_ns;
    int err = read_link_text("/proc/self/ns/user", &current_ns);
    if (err == 0) err = read_link_text("/proc/1/ns/user", &init_ns);
    if (err != 0 || current_ns == init_ns || prctl(PR_GET_KEEPCAPS) != 0) {
        *detail = "CLONE_NEWUSER|CLONE_VM inherited old user namespace or keepcaps";
        return 1;
    }
    return 0;
}

int run_clone_vm_userns_flow(std::string* detail) {
    if (prctl(PR_SET_KEEPCAPS, 1) != 0) {
        *detail = errno_detail("set keepcaps", errno);
        return 1;
    }
    return run_in_new_userns(verify_clone_vm_userns_child, nullptr, detail, CLONE_VM);
}

int run_id_and_groups_abi_flow(std::string* detail) {
    if (geteuid() != 0) return 0;

    gid_t groups[] = {3, 1, 2};
    if (setgroups(3, groups) != 0) {
        *detail = errno_detail("setgroups", errno);
        return 1;
    }
    gid_t result[5] = {99, 99, 99, 99, 99};
    if (getgroups(0, nullptr) != 3 || getgroups(5, result) != 3 ||
        result[0] != 1 || result[1] != 2 || result[2] != 3 ||
        result[3] != 99 || result[4] != 99) {
        *detail = "getgroups size/sort/copy semantics differ from Linux";
        return 1;
    }
    gid_t invalid_group = static_cast<gid_t>(-1);
    errno = 0;
    if (setgroups(1, &invalid_group) != -1 || errno != EINVAL || getgroups(5, result) != 3 ||
        result[0] != 1 || result[1] != 2 || result[2] != 3) {
        *detail = "invalid setgroups changed the group list or returned the wrong error";
        return 1;
    }
    errno = 0;
    if (syscall(SYS_getgroups, -1, result) != -1 || errno != EINVAL) {
        *detail = "negative getgroups size was not rejected";
        return 1;
    }

    if (syscall(SYS_setfsuid, 1000) != 0 || syscall(SYS_setfsuid, -1) != 1000) {
        *detail = "setfsuid did not update/read the FS UID";
        return 1;
    }
    if (setresuid(-1, -1, -1) != 0 || syscall(SYS_setfsuid, -1) != 1000) {
        *detail = "no-op setresuid changed the FS UID";
        return 1;
    }
    if (setreuid(-1, -1) != 0 || syscall(SYS_setfsuid, -1) != 0) {
        *detail = "setreuid did not reset the FS UID to the effective UID";
        return 1;
    }
    return 0;
}

int run_unshare_newuser_changes_namespace(std::string* detail) {
    std::string before;
    int err = read_link_text("/proc/self/ns/user", &before);
    if (err != 0) {
        *detail = errno_detail("readlink before unshare userns", err);
        return 1;
    }

    if (unshare(CLONE_NEWUSER) != 0) {
        *detail = errno_detail("unshare(CLONE_NEWUSER)", errno);
        return 1;
    }

    std::string after;
    err = read_link_text("/proc/self/ns/user", &after);
    if (err != 0) {
        *detail = errno_detail("readlink after unshare userns", err);
        return 1;
    }

    if (after == before) {
        *detail = "user namespace did not change after unshare: before='" + before + "' after='"
            + after + "'";
        return 1;
    }

    return 0;
}

int run_setns_userns_namespace_fd_success(std::string* detail) {
#ifndef SYS_setns
    *detail = "SYS_setns is not available";
    return 1;
#else
    std::string before;
    int err = read_link_text("/proc/self/ns/user", &before);
    if (err != 0) {
        *detail = errno_detail("readlink before setns userns", err);
        return 1;
    }

    HeldClone target;
    if (!spawn_held_clone(CLONE_NEWUSER, &target, detail)) {
        return 1;
    }

    int target_fd = open_userns_fd(target.pid, detail);
    if (target_fd < 0) {
        cleanup_held_clone(&target);
        return 1;
    }

    std::string target_ns;
    err = read_userns_link_for_pid(target.pid, &target_ns);
    if (err != 0) {
        close(target_fd);
        cleanup_held_clone(&target);
        *detail = errno_detail("readlink target userns", err);
        return 1;
    }

    if (syscall(SYS_setns, target_fd, CLONE_NEWUSER) != 0) {
        int sys_err = errno;
        close(target_fd);
        cleanup_held_clone(&target);
        *detail = errno_detail("setns(CLONE_NEWUSER)", sys_err);
        return 1;
    }

    std::string after;
    err = read_link_text("/proc/self/ns/user", &after);
    close(target_fd);
    cleanup_held_clone(&target);
    if (err != 0) {
        *detail = errno_detail("readlink after setns userns", err);
        return 1;
    }

    if (after != target_ns) {
        *detail = "setns entered unexpected user namespace: got='" + after + "' expected='"
            + target_ns + "'";
        return 1;
    }

    if (after == before) {
        *detail = "setns left user namespace unchanged: before='" + before + "' after='" + after
            + "'";
        return 1;
    }

    return 0;
#endif
}

int run_setns_userns_rejects_shared_fs(std::string* detail) {
#ifndef SYS_setns
    *detail = "SYS_setns is not available";
    return 1;
#else
    std::string before;
    int err = read_link_text("/proc/self/ns/user", &before);
    if (err != 0) {
        *detail = errno_detail("readlink before shared-fs setns", err);
        return 1;
    }

    HeldClone target;
    if (!spawn_held_clone(CLONE_NEWUSER, &target, detail)) {
        return 1;
    }

    int target_fd = open_userns_fd(target.pid, detail);
    if (target_fd < 0) {
        cleanup_held_clone(&target);
        return 1;
    }

    HeldClone shared_fs_holder;
    if (!spawn_held_clone(CLONE_FS, &shared_fs_holder, detail)) {
        close(target_fd);
        cleanup_held_clone(&target);
        return 1;
    }

    errno = 0;
    long ret = syscall(SYS_setns, target_fd, CLONE_NEWUSER);
    int setns_err = errno;

    std::string after;
    err = read_link_text("/proc/self/ns/user", &after);

    close(target_fd);
    cleanup_held_clone(&shared_fs_holder);
    cleanup_held_clone(&target);

    if (ret == 0) {
        *detail = "setns(CLONE_NEWUSER) unexpectedly succeeded while fs_struct was shared";
        return 1;
    }

    if (setns_err != EINVAL) {
        *detail = "setns(CLONE_NEWUSER) with shared fs returned errno=";
        *detail += std::to_string(setns_err);
        *detail += ", expected EINVAL(";
        *detail += std::to_string(EINVAL);
        *detail += ")";
        return 1;
    }

    if (err != 0) {
        *detail = errno_detail("readlink after shared-fs setns", err);
        return 1;
    }

    if (after != before) {
        *detail = "user namespace changed despite shared-fs rejection: before='" + before
            + "' after='" + after + "'";
        return 1;
    }

    return 0;
#endif
}

void expect_child_success(const char* case_name, int (*fn)(std::string*)) {
    int pipefd[2] = {-1, -1};
    ASSERT_EQ(0, pipe(pipefd)) << case_name << ": pipe failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    pid_t child = fork();
    ASSERT_GE(child, 0) << case_name << ": fork failed: errno=" << errno << " ("
                        << strerror(errno) << ")";

    if (child == 0) {
        close(pipefd[0]);
        std::string detail;
        int rc = fn(&detail);
        send_detail(pipefd[1], detail);
        close(pipefd[1]);
        _exit(rc);
    }

    close(pipefd[1]);

    int status = 0;
    std::string wait_detail;
    ASSERT_TRUE(wait_for_child_exit(child, &status, kChildWaitTimeoutSec, true, &wait_detail))
        << case_name << ": " << wait_detail;

    std::string detail = read_pipe_detail(pipefd[0]);
    close(pipefd[0]);

    ASSERT_TRUE(WIFEXITED(status)) << case_name << ": child terminated abnormally";
    EXPECT_EQ(0, WEXITSTATUS(status)) << case_name << ": " << detail;
}

TEST(UserNamespaceIdMap, InitialNamespaceMapsAreReadable) {
    std::string self_user_ns;
    std::string init_user_ns;
    ASSERT_EQ(0, read_link_text("/proc/self/ns/user", &self_user_ns));
    ASSERT_EQ(0, read_link_text("/proc/1/ns/user", &init_user_ns));
    ASSERT_EQ(self_user_ns, init_user_ns);

    for (const char* path : {"/proc/self/uid_map", "/proc/self/gid_map"}) {
        std::string content;
        ASSERT_EQ(0, read_text_file(path, &content)) << path;
        EXPECT_FALSE(content.empty()) << path;
    }
}

TEST(UserNamespaceIdMap, RootlessSingleAndNestedSelfMaps) {
    expect_child_success("rootless_nested_id_map_flow", run_rootless_nested_id_map_flow);
}

TEST(UserNamespaceIdMap, ChildCannotBridgeDiscontinuousParentMap) {
    expect_child_success("discontinuous_parent_map_flow", run_discontinuous_parent_map_flow);
}

TEST(UserNamespaceIdMap, PreopenedMapFdDoesNotRetainWriterPrivilege) {
    expect_child_success("dropped_map_writer_flow", run_dropped_map_writer_flow);
}

TEST(UserNamespaceIdMap, MapFdPinsNamespaceAfterTargetExit) {
    expect_child_success("map_fd_after_target_exit_flow", run_map_fd_after_target_exit_flow);
}

TEST(UserNamespaceIdMap, SetgroupsWriteRequiresAdminAtOpen) {
    expect_child_success("setgroups_open_capability_flow", run_setgroups_open_capability_flow);
}

TEST(UserNamespaceIdMap, CloneVmStillCreatesNewUserNamespace) {
    expect_child_success("clone_vm_userns_flow", run_clone_vm_userns_flow);
}

TEST(UserNamespaceIdMap, CredentialAndGroupsAbi) {
    expect_child_success("id_and_groups_abi_flow", run_id_and_groups_abi_flow);
}

TEST(UserNamespaceControl, UnshareNewUserChangesNamespace) {
    expect_child_success("unshare_newuser_changes_namespace", run_unshare_newuser_changes_namespace);
}

TEST(UserNamespaceControl, NamespaceFdSetnsSucceedsWhenFsExclusive) {
    expect_child_success("setns_userns_namespace_fd_success", run_setns_userns_namespace_fd_success);
}

TEST(UserNamespaceControl, NamespaceFdSetnsRejectsSharedFs) {
    expect_child_success("setns_userns_rejects_shared_fs", run_setns_userns_rejects_shared_fs);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
