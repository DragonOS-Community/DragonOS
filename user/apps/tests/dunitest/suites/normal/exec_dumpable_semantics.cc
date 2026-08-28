// exec would_dump() regression: an executable which is executable but not
// readable must force the new mm non-dumpable.

#include <errno.h>
#include <stdint.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#include <gtest/gtest.h>

namespace {

constexpr uid_t kTestId = 1000;
constexpr char kProbeFdEnv[] = "DUNITEST_EXEC_DUMPABLE_FD";
constexpr char kResumeFdEnv[] = "DUNITEST_EXEC_DUMPABLE_RESUME_FD";
constexpr int kProbeValue = 0x2198a55a;

struct ExecProbeReady {
    int dumpable;
    uintptr_t address;
};

struct ExecProbeObservation {
    int dumpable;
    ssize_t access_count;
    int access_errno;
    int value;
};

bool read_exact(int fd, void* buffer, size_t length) {
    size_t done = 0;
    while (done < length) {
        const ssize_t count = read(fd, static_cast<char*>(buffer) + done, length - done);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) return false;
        done += static_cast<size_t>(count);
    }
    return true;
}

bool write_exact(int fd, const void* buffer, size_t length) {
    size_t done = 0;
    while (done < length) {
        const ssize_t count = write(fd, static_cast<const char*>(buffer) + done, length - done);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) return false;
        done += static_cast<size_t>(count);
    }
    return true;
}

bool copy_self_to(int destination) {
    int source = open("/proc/self/exe", O_RDONLY);
    if (source < 0) return false;
    char buffer[16384];
    for (;;) {
        const ssize_t count = read(source, buffer, sizeof(buffer));
        if (count == 0) break;
        if (count < 0) {
            close(source);
            return false;
        }
        ssize_t done = 0;
        while (done < count) {
            const ssize_t written = write(destination, buffer + done, count - done);
            if (written <= 0) {
                close(source);
                return false;
            }
            done += written;
        }
    }
    close(source);
    return true;
}

int exec_probe(mode_t mode, ExecProbeObservation* observation) {
    char path[] = "/tmp/dunitest_exec_dumpable_XXXXXX";
    int executable = mkstemp(path);
    if (executable < 0) return errno;
    if (!copy_self_to(executable) || fchown(executable, kTestId, kTestId) != 0 ||
        fchmod(executable, mode) != 0) {
        const int error = errno != 0 ? errno : EIO;
        close(executable);
        unlink(path);
        return error;
    }
    close(executable);

    int result_pipe[2];
    if (pipe(result_pipe) != 0) {
        const int error = errno;
        unlink(path);
        return error;
    }

    const pid_t supervisor = fork();
    if (supervisor == 0) {
        close(result_pipe[0]);
        int ready_pipe[2];
        int resume_pipe[2];
        if (pipe(ready_pipe) != 0 || pipe(resume_pipe) != 0) _exit(59);

        char ready_fd[32];
        char resume_fd[32];
        snprintf(ready_fd, sizeof(ready_fd), "%d", ready_pipe[1]);
        snprintf(resume_fd, sizeof(resume_fd), "%d", resume_pipe[0]);
        if (setenv(kProbeFdEnv, ready_fd, 1) != 0 ||
            setenv(kResumeFdEnv, resume_fd, 1) != 0 || setgid(kTestId) != 0 ||
            setuid(kTestId) != 0 || prctl(PR_SET_DUMPABLE, 1, 0, 0, 0) != 0) {
            _exit(60);
        }

        const pid_t tracee = fork();
        if (tracee == 0) {
            close(result_pipe[1]);
            close(ready_pipe[0]);
            close(resume_pipe[1]);
            execl(path, path, static_cast<char*>(nullptr));
            _exit(62);
        }
        close(ready_pipe[1]);
        close(resume_pipe[0]);
        if (tracee < 0) _exit(61);

        ExecProbeReady ready = {};
        if (!read_exact(ready_pipe[0], &ready, sizeof(ready))) _exit(63);
        close(ready_pipe[0]);
        ExecProbeObservation child_observation = {};
        child_observation.dumpable = ready.dumpable;
        iovec local = {&child_observation.value, sizeof(child_observation.value)};
        iovec remote = {reinterpret_cast<void*>(ready.address), sizeof(child_observation.value)};
        errno = 0;
        child_observation.access_count =
            syscall(SYS_process_vm_readv, tracee, &local, 1, &remote, 1, 0);
        child_observation.access_errno =
            child_observation.access_count < 0 ? errno : 0;

        const char resume = 1;
        if (!write_exact(resume_pipe[1], &resume, sizeof(resume))) _exit(64);
        close(resume_pipe[1]);
        int tracee_status = 0;
        if (waitpid(tracee, &tracee_status, 0) != tracee || !WIFEXITED(tracee_status) ||
            WEXITSTATUS(tracee_status) != 0) {
            _exit(65);
        }
        if (!write_exact(result_pipe[1], &child_observation, sizeof(child_observation))) _exit(66);
        close(result_pipe[1]);
        _exit(0);
    }

    close(result_pipe[1]);
    ExecProbeObservation value = {};
    const bool received = read_exact(result_pipe[0], &value, sizeof(value));
    close(result_pipe[0]);
    int status = 0;
    const pid_t waited = supervisor > 0 ? waitpid(supervisor, &status, 0) : -1;
    const int fork_error = supervisor < 0 ? errno : 0;
    unlink(path);
    if (supervisor < 0) return fork_error;
    if (waited != supervisor || !WIFEXITED(status) || WEXITSTATUS(status) != 0 || !received) {
        return EIO;
    }
    *observation = value;
    return 0;
}

int configured_suid_dumpable() {
    int fd = open("/proc/sys/fs/suid_dumpable", O_RDONLY);
    if (fd < 0) return 0;
    char value[16] = {};
    const ssize_t count = read(fd, value, sizeof(value) - 1);
    close(fd);
    return count > 0 ? atoi(value) : 0;
}

}  // namespace

TEST(ExecDumpableSemantics, ReadPermissionControlsNewMmDumpability) {
    if (geteuid() != 0) GTEST_SKIP() << "requires root to construct uid 1000 executable";

    ExecProbeObservation readable = {};
    ASSERT_EQ(0, exec_probe(0500, &readable));
    EXPECT_EQ(1, readable.dumpable) << "owner-readable executable should remain dumpable";
    EXPECT_EQ(static_cast<ssize_t>(sizeof(readable.value)), readable.access_count);
    EXPECT_EQ(0, readable.access_errno);
    EXPECT_EQ(kProbeValue, readable.value);

    ExecProbeObservation execute_only = {};
    ASSERT_EQ(0, exec_probe(0100, &execute_only));
    EXPECT_EQ(configured_suid_dumpable(), execute_only.dumpable)
        << "execute-only executable must use the configured suid_dumpable policy";
    if (execute_only.dumpable == 1) {
        EXPECT_EQ(static_cast<ssize_t>(sizeof(execute_only.value)), execute_only.access_count);
        EXPECT_EQ(0, execute_only.access_errno);
        EXPECT_EQ(kProbeValue, execute_only.value);
    } else {
        EXPECT_EQ(-1, execute_only.access_count);
        EXPECT_EQ(EPERM, execute_only.access_errno)
            << "a same-uid parent must not read a non-dumpable post-exec mm";
    }
}

int main(int argc, char** argv) {
    if (const char* fd_string = getenv(kProbeFdEnv)) {
        const int fd = atoi(fd_string);
        const char* resume_string = getenv(kResumeFdEnv);
        if (resume_string == nullptr) _exit(67);
        const int resume_fd = atoi(resume_string);
        volatile int probe_value = kProbeValue;
        const int dumpable = prctl(PR_GET_DUMPABLE, 0, 0, 0, 0);
        const ExecProbeReady ready = {
            dumpable,
            reinterpret_cast<uintptr_t>(const_cast<int*>(&probe_value)),
        };
        char resume = 0;
        const bool ok = dumpable >= 0 && write_exact(fd, &ready, sizeof(ready)) &&
                        read_exact(resume_fd, &resume, sizeof(resume));
        _exit(ok ? 0 : 68);
    }
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
