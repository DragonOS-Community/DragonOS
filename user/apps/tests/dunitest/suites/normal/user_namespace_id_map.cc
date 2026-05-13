#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
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

int run_in_new_userns(UserNsRunner runner, void* arg, std::string* detail) {
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

    pid_t child = clone(clone_runner_entry, stack.data() + stack.size(), CLONE_NEWUSER | SIGCHLD,
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

    int err = write_text_file("/proc/self/uid_map", "0 0 1\n");
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

    int err = write_text_file("/proc/self/uid_map", first_uid_map);
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

TEST(UserNamespaceIdMap, RootlessSingleAndNestedSelfMaps) {
    expect_child_success("rootless_nested_id_map_flow", run_rootless_nested_id_map_flow);
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
