#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#include "cap_common.h"

namespace {

constexpr const char* kOomScoreAdjPath = "/proc/self/oom_score_adj";
constexpr size_t kCloneStackSize = 64 * 1024;

int write_oom_score_adj_errno(int score) {
    char buf[32];
    int len = snprintf(buf, sizeof(buf), "%d\n", score);
    if (len <= 0 || len >= static_cast<int>(sizeof(buf))) {
        return EINVAL;
    }

    int fd = open(kOomScoreAdjPath, O_WRONLY);
    if (fd < 0) {
        return errno;
    }

    errno = 0;
    ssize_t written = write(fd, buf, len);
    int saved_errno = errno;
    close(fd);
    if (written < 0) {
        return saved_errno;
    }
    if (written != len) {
        return EIO;
    }
    return 0;
}

int read_oom_score_adj(int* out) {
    char buf[32] = {};
    int fd = open(kOomScoreAdjPath, O_RDONLY);
    if (fd < 0) {
        return errno;
    }

    errno = 0;
    ssize_t nread = read(fd, buf, sizeof(buf) - 1);
    int saved_errno = errno;
    close(fd);
    if (nread < 0) {
        return saved_errno;
    }
    char* end = nullptr;
    long value = strtol(buf, &end, 10);
    if (end == buf) {
        return EINVAL;
    }
    *out = static_cast<int>(value);
    return 0;
}

int drop_all_caps() {
    cap_user_data_t zeros[2];
    fill_caps_v3(0, 0, 0, zeros);
    return capset_errno(_LINUX_CAPABILITY_VERSION_3, 0, zeros);
}

int expect_current_score(int expected) {
    int value = 0;
    int err = read_oom_score_adj(&value);
    if (err != 0) {
        return 10 + err;
    }
    return value == expected ? 0 : 20;
}

int write_exact(int fd, const void* buf, size_t len) {
    const char* cursor = static_cast<const char*>(buf);
    while (len > 0) {
        ssize_t written = write(fd, cursor, len);
        if (written < 0) {
            return errno;
        }
        if (written == 0) {
            return EIO;
        }
        cursor += written;
        len -= static_cast<size_t>(written);
    }
    return 0;
}

int read_exact(int fd, void* buf, size_t len) {
    char* cursor = static_cast<char*>(buf);
    while (len > 0) {
        ssize_t nread = read(fd, cursor, len);
        if (nread < 0) {
            return errno;
        }
        if (nread == 0) {
            return EPIPE;
        }
        cursor += nread;
        len -= static_cast<size_t>(nread);
    }
    return 0;
}

void expect_child_success(int (*child_fn)(), const char* name) {
    pid_t child = fork();
    ASSERT_GE(child, 0) << name << ": fork failed: errno=" << errno << " (" << strerror(errno)
                        << ")";
    if (child == 0) {
        _exit(child_fn());
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0))
        << name << ": waitpid failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_TRUE(WIFEXITED(status)) << name << ": child did not exit normally, status=0x"
                                   << std::hex << status;
    EXPECT_EQ(0, WEXITSTATUS(status)) << name << ": child exit status=0x" << std::hex
                                      << WEXITSTATUS(status);
}

int child_unprivileged_can_raise_negative_oom_score_adj() {
    if (write_oom_score_adj_errno(-1000) != 0) {
        return 1;
    }
    if (drop_all_caps() != 0) {
        return 2;
    }
    if (write_oom_score_adj_errno(-500) != 0) {
        return 3;
    }
    return expect_current_score(-500);
}

int child_unprivileged_cannot_lower_below_min() {
    if (write_oom_score_adj_errno(-500) != 0) {
        return 1;
    }
    if (drop_all_caps() != 0) {
        return 2;
    }
    if (write_oom_score_adj_errno(-1000) != EACCES) {
        return 3;
    }
    return expect_current_score(-500);
}

int child_capable_write_updates_min() {
    if (write_oom_score_adj_errno(-200) != 0) {
        return 1;
    }
    if (drop_all_caps() != 0) {
        return 2;
    }
    if (write_oom_score_adj_errno(-300) != EACCES) {
        return 3;
    }
    return expect_current_score(-200);
}

int child_default_unprivileged_cannot_set_negative_oom_score_adj() {
    if (expect_current_score(0) != 0) {
        return 1;
    }
    if (drop_all_caps() != 0) {
        return 2;
    }
    if (write_oom_score_adj_errno(-1) != EACCES) {
        return 3;
    }
    return expect_current_score(0);
}

struct ThreadForkOomArgs {
    int ready_write;
    int go_read;
    int result;
};

void* thread_fork_oom_score_adj_child(void* arg) {
    auto* args = static_cast<ThreadForkOomArgs*>(arg);
    char byte = 'x';
    args->result = write_exact(args->ready_write, &byte, 1);
    if (args->result != 0) {
        return nullptr;
    }
    args->result = read_exact(args->go_read, &byte, 1);
    if (args->result != 0) {
        return nullptr;
    }

    pid_t child = fork();
    if (child < 0) {
        args->result = 100 + errno;
        return nullptr;
    }
    if (child == 0) {
        _exit(expect_current_score(-700));
    }

    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        args->result = 200 + errno;
        return nullptr;
    }
    if (!WIFEXITED(status)) {
        args->result = 300;
        return nullptr;
    }
    args->result = WEXITSTATUS(status);
    return nullptr;
}

int child_non_leader_fork_copies_leader_oom_score_adj() {
    int ready_pipe[2] = {-1, -1};
    int go_pipe[2] = {-1, -1};
    if (pipe(ready_pipe) != 0) {
        return 1;
    }
    if (pipe(go_pipe) != 0) {
        close(ready_pipe[0]);
        close(ready_pipe[1]);
        return 2;
    }

    ThreadForkOomArgs args = {};
    args.ready_write = ready_pipe[1];
    args.go_read = go_pipe[0];
    pthread_t thread = {};
    int rc = pthread_create(&thread, nullptr, thread_fork_oom_score_adj_child, &args);
    if (rc != 0) {
        close(ready_pipe[0]);
        close(ready_pipe[1]);
        close(go_pipe[0]);
        close(go_pipe[1]);
        return 10 + rc;
    }

    char byte = 0;
    int err = read_exact(ready_pipe[0], &byte, 1);
    if (err == 0) {
        err = write_oom_score_adj_errno(-700);
    }
    if (err == 0) {
        err = write_exact(go_pipe[1], &byte, 1);
    }

    void* thread_ret = nullptr;
    int join_err = pthread_join(thread, &thread_ret);
    close(ready_pipe[0]);
    close(ready_pipe[1]);
    close(go_pipe[0]);
    close(go_pipe[1]);
    if (err != 0) {
        return 400 + err;
    }
    if (join_err != 0) {
        return 500 + join_err;
    }
    return args.result;
}

int child_vfork_write_does_not_propagate_to_parent() {
    pid_t child = vfork();
    if (child < 0) {
        return 1;
    }
    if (child == 0) {
        int err = write_oom_score_adj_errno(-600);
        _exit(err == 0 ? 0 : 10 + err);
    }

    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        return 100 + errno;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        return 200;
    }
    return expect_current_score(0);
}

struct CloneVmOomArgs {
    int go_read;
};

int clone_vm_oom_child(void* arg) {
    auto* args = static_cast<CloneVmOomArgs*>(arg);
    char byte = 0;
    int err = read_exact(args->go_read, &byte, 1);
    if (err != 0) {
        _exit(10 + err);
    }
    _exit(expect_current_score(-400));
}

int child_clone_vm_process_receives_shared_mm_oom_score_adj_update() {
    int go_pipe[2] = {-1, -1};
    if (pipe(go_pipe) != 0) {
        return 1;
    }

    void* stack = malloc(kCloneStackSize);
    if (stack == nullptr) {
        close(go_pipe[0]);
        close(go_pipe[1]);
        return 2;
    }

    CloneVmOomArgs args = {};
    args.go_read = go_pipe[0];
    auto* stack_top = static_cast<char*>(stack) + kCloneStackSize;
    pid_t child = clone(clone_vm_oom_child, stack_top, CLONE_VM | SIGCHLD, &args);
    if (child < 0) {
        int err = errno;
        free(stack);
        close(go_pipe[0]);
        close(go_pipe[1]);
        return 10 + err;
    }

    int err = write_oom_score_adj_errno(-400);
    char byte = 'x';
    if (err == 0) {
        err = write_exact(go_pipe[1], &byte, 1);
    }

    int status = 0;
    int wait_err = 0;
    if (waitpid(child, &status, 0) != child) {
        wait_err = errno;
    }
    free(stack);
    close(go_pipe[0]);
    close(go_pipe[1]);
    if (err != 0) {
        return 100 + err;
    }
    if (wait_err != 0) {
        return 200 + wait_err;
    }
    if (!WIFEXITED(status)) {
        return 300;
    }
    return WEXITSTATUS(status);
}

}  // namespace

TEST(OomScoreAdj, UnprivilegedCanRaiseNegativeOomScoreAdj) {
    expect_child_success(child_unprivileged_can_raise_negative_oom_score_adj,
                         "UnprivilegedCanRaiseNegativeOomScoreAdj");
}

TEST(OomScoreAdj, UnprivilegedCannotLowerBelowMin) {
    expect_child_success(child_unprivileged_cannot_lower_below_min,
                         "UnprivilegedCannotLowerBelowMin");
}

TEST(OomScoreAdj, CapableWriteUpdatesMin) {
    expect_child_success(child_capable_write_updates_min, "CapableWriteUpdatesMin");
}

TEST(OomScoreAdj, DefaultUnprivilegedCannotSetNegativeOomScoreAdj) {
    expect_child_success(child_default_unprivileged_cannot_set_negative_oom_score_adj,
                         "DefaultUnprivilegedCannotSetNegativeOomScoreAdj");
}

TEST(OomScoreAdj, NonLeaderForkCopiesLeaderOomScoreAdj) {
    expect_child_success(child_non_leader_fork_copies_leader_oom_score_adj,
                         "NonLeaderForkCopiesLeaderOomScoreAdj");
}

TEST(OomScoreAdj, VforkWriteDoesNotPropagateToParent) {
    expect_child_success(child_vfork_write_does_not_propagate_to_parent,
                         "VforkWriteDoesNotPropagateToParent");
}

TEST(OomScoreAdj, CloneVmProcessReceivesSharedMmOomScoreAdjUpdate) {
    expect_child_success(child_clone_vm_process_receives_shared_mm_oom_score_adj_update,
                         "CloneVmProcessReceivesSharedMmOomScoreAdjUpdate");
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
