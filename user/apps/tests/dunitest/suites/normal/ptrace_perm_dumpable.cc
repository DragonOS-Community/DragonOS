// Regression test for the interplay between ptrace permissions and dumpability.
//
// Semantics covered:
// 1. After the tracee drops privileges via setgid+setuid, dumpable resets to zero,
//    and ATTACH from a same-uid, unprivileged tracer must be rejected (EPERM);
// 2. After the tracee calls prctl(PR_SET_DUMPABLE, 1), the same tracer ATTACH succeeds,
//    stop events are reported normally, and DETACH exits cleanly;
// 3. Dumpability does not affect TRACEME (voluntary tracing relies only on the capability check).
//
// dunitest runs as root: privilege dropping only happens inside forked children, while the
// parent stays root and handles cleanup.

#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <gtest/gtest.h>

// ptrace request numbers (fallback when build-environment headers are missing; values match the Linux x86_64 ABI)
#ifndef PTRACE_TRACEME
#define PTRACE_TRACEME 0
#endif
#ifndef PTRACE_ATTACH
#define PTRACE_ATTACH 16
#endif
#ifndef PTRACE_DETACH
#define PTRACE_DETACH 17
#endif
#ifndef PR_SET_DUMPABLE
#define PR_SET_DUMPABLE 4
#endif

namespace {

constexpr int kTestUid = 1000;

long ptrace_call(long request, long pid, unsigned long addr, unsigned long data) {
    return syscall(SYS_ptrace, request, pid, addr, data);
}

// Drop privileges to kTestUid: lower gid and uid together so they match the tracee's gid six-tuple.
// Returns 0 on success, errno on failure.
int drop_to_test_ids() {
    if (setgid(kTestUid) != 0) {
        return errno;
    }
    if (setuid(kTestUid) != 0) {
        return errno;
    }
    return 0;
}

bool write_byte(int fd, char value) {
    return write(fd, &value, 1) == 1;
}

bool read_byte(int fd, char* out) {
    return read(fd, out, 1) == 1;
}

struct DumpableThreadSync {
    pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
    pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
    int phase = 0;
    int observed = -1;
};

void* dumpable_thread(void* opaque) {
    auto* sync = static_cast<DumpableThreadSync*>(opaque);
    pthread_mutex_lock(&sync->lock);
    while (sync->phase == 0) {
        pthread_cond_wait(&sync->cond, &sync->lock);
    }
    pthread_mutex_unlock(&sync->lock);

    sync->observed = prctl(PR_GET_DUMPABLE, 0, 0, 0, 0);
    if (sync->observed == 0) {
        prctl(PR_SET_DUMPABLE, 1, 0, 0, 0);
    }

    pthread_mutex_lock(&sync->lock);
    sync->phase = 2;
    pthread_cond_signal(&sync->cond);
    pthread_mutex_unlock(&sync->lock);
    return nullptr;
}

}  // namespace

TEST(PtracePermDumpable, DumpabilityIsSharedByThreadsInOneMm) {
    ASSERT_EQ(0, prctl(PR_SET_DUMPABLE, 0, 0, 0, 0));

    DumpableThreadSync sync;
    pthread_t thread;
    ASSERT_EQ(0, pthread_create(&thread, nullptr, dumpable_thread, &sync));

    pthread_mutex_lock(&sync.lock);
    sync.phase = 1;
    pthread_cond_signal(&sync.cond);
    while (sync.phase != 2) {
        pthread_cond_wait(&sync.cond, &sync.lock);
    }
    pthread_mutex_unlock(&sync.lock);

    ASSERT_EQ(0, pthread_join(thread, nullptr));
    EXPECT_EQ(0, sync.observed);
    EXPECT_EQ(1, prctl(PR_GET_DUMPABLE, 0, 0, 0, 0));
}

TEST(PtracePermDumpable, DumpabilityGatesAttachButNotTraceme) {
    // ---- Scenario C: dumpable=0 does not affect TRACEME ----
    // The parent runs as root (holds CAP_SYS_PTRACE); TRACEME from the privilege-dropped child must succeed.
    {
        pid_t child = fork();
        ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
        if (child == 0) {
            if (geteuid() != 0) {
                // Without root the privilege-drop scenario cannot be constructed; skip (following the capability.cc precedent).
                _exit(0);
            }
            if (drop_to_test_ids() != 0) {
                _exit(60);
            }
            // At this point dumpable has already been reset to zero by the setuid privilege drop.
            if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) {
                _exit(61);
            }
            _exit(0);
        }
        int status = 0;
        ASSERT_EQ(child, waitpid(child, &status, 0));
        ASSERT_TRUE(WIFEXITED(status)) << "child status=" << status;
        EXPECT_EQ(0, WEXITSTATUS(status)) << "TRACEME should succeed with dumpable=0";
    }

    // ---- Scenario A/B: ATTACH is first rejected with dumpable=0, then succeeds after PR_SET_DUMPABLE(1) ----
    // Structure: the parent forks a tracer; the tracer forks the tracee (the tracee is the tracer's child,
    // so the tracer can waitpid its stop events).
    int tracee_to_tracer[2];
    int tracer_to_tracee[2];
    ASSERT_EQ(0, pipe(tracee_to_tracer));
    ASSERT_EQ(0, pipe(tracer_to_tracee));

    pid_t tracer = fork();
    ASSERT_GE(tracer, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
    if (tracer == 0) {
        if (geteuid() != 0) {
            // Without root, setuid(1000) is not a privilege drop (dumpable will not reset to zero),
            // so the scenario cannot be constructed; skip (following the capability.cc precedent).
            _exit(0);
        }
        // Note: fork first, then each side closes the ends it does not use — if closed before the fork,
        pid_t tracee = fork();
        if (tracee < 0) {
            _exit(90);
        }
        if (tracee == 0) {
            // The tracee writes via tracee_to_tracer[1] and reads via tracer_to_tracee[0].
            close(tracee_to_tracer[0]);
            close(tracer_to_tracee[1]);
            // The tracee: drops privileges (dumpable→0), notifies, then waits for the "set dumpable=1" command.
            if (drop_to_test_ids() != 0) {
                _exit(40);
            }
            if (!write_byte(tracee_to_tracer[1], 'r')) {
                _exit(41);
            }
            char cmd = 0;
            if (!read_byte(tracer_to_tracee[0], &cmd) || cmd != 'g') {
                _exit(42);
            }
            if (prctl(PR_SET_DUMPABLE, 1, 0, 0, 0) != 0) {
                _exit(43);
            }
            if (!write_byte(tracee_to_tracer[1], 'd')) {
                _exit(44);
            }
            // Wait to be stopped by an attach or to be cleaned up; do not exit on our own.
            for (;;) {
                pause();
            }
        }

        // The tracer reads via tracee_to_tracer[0] and writes via tracer_to_tracee[1].
        close(tracee_to_tracer[1]);
        close(tracer_to_tracee[0]);


        // The tracer: drops privileges to the same identity as the tracee.
        int err = drop_to_test_ids();
        if (err != 0) {
            kill(tracee, SIGKILL);
            _exit(50 + (err > 0 && err < 10 ? err : 9));
        }

        char c = 0;
        // The tracee has dropped privileges and dumpable=0.
        if (!read_byte(tracee_to_tracer[0], &c)) {
            kill(tracee, SIGKILL);
            _exit(20);
        }

        errno = 0;
        if (ptrace_call(PTRACE_ATTACH, tracee, 0, 0) != -1) {
            kill(tracee, SIGKILL);
            _exit(21);  // must not be allowed when dumpable=0
        }
        if (errno != EPERM) {
            kill(tracee, SIGKILL);
            _exit(22);  // expected EPERM
        }

        // Tell the tracee to set dumpable=1, then attach again.
        if (!write_byte(tracer_to_tracee[1], 'g')) {
            kill(tracee, SIGKILL);
            _exit(23);
        }
        if (!read_byte(tracee_to_tracer[0], &c)) {
            kill(tracee, SIGKILL);
            _exit(24);
        }

        if (ptrace_call(PTRACE_ATTACH, tracee, 0, 0) != 0) {
            kill(tracee, SIGKILL);
            _exit(25);  // should succeed after dumpable=1
        }
        int status = 0;
        if (waitpid(tracee, &status, WUNTRACED) != tracee || !WIFSTOPPED(status)) {
            kill(tracee, SIGKILL);
            _exit(26);  // should observe a stop after attach
        }
        if (ptrace_call(PTRACE_DETACH, tracee, 0, 0) != 0) {
            kill(tracee, SIGKILL);
            _exit(27);
        }
        if (kill(tracee, SIGKILL) != 0) {
            _exit(28);
        }
        if (waitpid(tracee, &status, 0) != tracee || !WIFSIGNALED(status) ||
            WTERMSIG(status) != SIGKILL) {
            _exit(29);
        }
        _exit(0);
    }

    close(tracee_to_tracer[0]);
    close(tracee_to_tracer[1]);
    close(tracer_to_tracee[0]);
    close(tracer_to_tracee[1]);

    int status = 0;
    ASSERT_EQ(tracer, waitpid(tracer, &status, 0));
    ASSERT_TRUE(WIFEXITED(status)) << "tracer status=" << status;
    const int code = WEXITSTATUS(status);
    EXPECT_EQ(0, code) << "tracer exit code meaning: 20/24=pipe sync failed 21=ATTACH not rejected with dumpable=0"
                          " 22=errno not EPERM 25=ATTACH failed after dumpable=1 26=no stop observed"
                          " 27=DETACH failed 28/29=cleanup failed 4x=tracee setup failed 5x=tracer privilege drop failed"
                          " 60/61=TRACEME scenario failed 90=fork failed";
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
