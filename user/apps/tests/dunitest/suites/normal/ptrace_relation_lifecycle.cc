// Ptrace relation ownership and teardown lifecycle regression tests.

#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <chrono>

#include <gtest/gtest.h>

#ifndef PTRACE_SEIZE
#define PTRACE_SEIZE 0x4206
#endif
#ifndef PTRACE_DETACH
#define PTRACE_DETACH 17
#endif
#ifndef PTRACE_SYSCALL
#define PTRACE_SYSCALL 24
#endif
#ifndef PTRACE_INTERRUPT
#define PTRACE_INTERRUPT 0x4207
#endif
#ifndef PTRACE_O_TRACESYSGOOD
#define PTRACE_O_TRACESYSGOOD 0x00000001
#endif
#ifndef PTRACE_O_EXITKILL
#define PTRACE_O_EXITKILL 0x00100000
#endif
#ifndef __WALL
#define __WALL 0x40000000
#endif

namespace {

struct OpResult {
    pid_t reporter;
    long result;
    int error;
    int status;
};

long ptrace_call(long request, pid_t pid, unsigned long addr,
                 unsigned long data) {
    return syscall(SYS_ptrace, request, pid, addr, data);
}

bool WriteExact(int fd, const void* buffer, size_t size) {
    const char* cursor = static_cast<const char*>(buffer);
    while (size > 0) {
        const ssize_t written = write(fd, cursor, size);
        if (written > 0) {
            cursor += written;
            size -= static_cast<size_t>(written);
        } else if (written < 0 && errno == EINTR) {
            continue;
        } else {
            return false;
        }
    }
    return true;
}

bool ReadExactUntil(int fd, void* buffer, size_t size,
                    std::chrono::milliseconds timeout =
                        std::chrono::seconds(3)) {
    char* cursor = static_cast<char*>(buffer);
    const auto deadline = std::chrono::steady_clock::now() + timeout;
    while (size > 0) {
        const auto now = std::chrono::steady_clock::now();
        if (now >= deadline) {
            errno = ETIMEDOUT;
            return false;
        }
        const auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(
                                   deadline - now)
                                   .count();
        pollfd event = {};
        event.fd = fd;
        event.events = POLLIN;
        const int ready = poll(&event, 1, static_cast<int>(remaining) + 1);
        if (ready < 0 && errno == EINTR) {
            continue;
        }
        if (ready <= 0 || (event.revents & (POLLIN | POLLHUP)) == 0) {
            if (ready == 0) {
                errno = ETIMEDOUT;
            }
            return false;
        }
        const ssize_t got = read(fd, cursor, size);
        if (got > 0) {
            cursor += got;
            size -= static_cast<size_t>(got);
        } else if (got < 0 && errno == EINTR) {
            continue;
        } else {
            return false;
        }
    }
    return true;
}

bool WaitPidUntil(pid_t child, int* status, int options = 0,
                  std::chrono::milliseconds timeout =
                      std::chrono::seconds(3)) {
    const auto deadline = std::chrono::steady_clock::now() + timeout;
    for (;;) {
        const pid_t result = waitpid(child, status, options | WNOHANG);
        if (result == child) {
            return true;
        }
        if (result < 0 && errno != EINTR) {
            return false;
        }
        if (std::chrono::steady_clock::now() >= deadline) {
            errno = ETIMEDOUT;
            return false;
        }
        sched_yield();
    }
}

class ProcessGuard {
public:
    explicit ProcessGuard(pid_t child = -1) : child_(child) {}
    ProcessGuard(const ProcessGuard&) = delete;
    ProcessGuard& operator=(const ProcessGuard&) = delete;
    ~ProcessGuard() {
        if (child_ > 0) {
            kill(child_, SIGKILL);
            int status = 0;
            WaitPidUntil(child_, &status);
        }
    }
    void Release() { child_ = -1; }

private:
    pid_t child_;
};

[[noreturn]] void RunSeizeContender(pid_t tracee, int ready_fd, int start_fd,
                                    int result_fd, int command_fd) {
    const pid_t self = getpid();
    if (!WriteExact(ready_fd, &self, sizeof(self))) {
        _exit(80);
    }
    char token = 0;
    if (!ReadExactUntil(start_fd, &token, sizeof(token))) {
        _exit(81);
    }

    errno = 0;
    OpResult seize = {self, ptrace_call(PTRACE_SEIZE, tracee, 0, 0), errno, 0};
    if (seize.result < 0) {
        seize.error = errno;
    }
    if (!WriteExact(result_fd, &seize, sizeof(seize)) ||
        !ReadExactUntil(command_fd, &token, sizeof(token))) {
        _exit(82);
    }

    OpResult detach = {self, 0, 0, 0};
    if (seize.result == 0) {
        detach.result = ptrace_call(PTRACE_INTERRUPT, tracee, 0, 0);
        if (detach.result == 0 &&
            !WaitPidUntil(tracee, &detach.status, __WALL)) {
            detach.result = -1;
        }
        if (detach.result == 0) {
            detach.result = ptrace_call(PTRACE_DETACH, tracee, 0, 0);
        }
        if (detach.result < 0) {
            detach.error = errno;
        }
    }
    WriteExact(result_fd, &detach, sizeof(detach));
    _exit(detach.result == 0 ? 0 : 83);
}

TEST(PtraceRelationLifecycle, ConcurrentSeizeHasExactlyOneOwner) {
    pid_t tracee = fork();
    ASSERT_GE(tracee, 0);
    if (tracee == 0) {
        for (;;) {
            pause();
        }
    }
    ProcessGuard tracee_guard(tracee);

    int ready[2], start[2], results[2], command_a[2], command_b[2];
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(start));
    ASSERT_EQ(0, pipe(results));
    ASSERT_EQ(0, pipe(command_a));
    ASSERT_EQ(0, pipe(command_b));

    pid_t tracer_a = fork();
    ASSERT_GE(tracer_a, 0);
    if (tracer_a == 0) {
        RunSeizeContender(tracee, ready[1], start[0], results[1], command_a[0]);
    }
    ProcessGuard tracer_a_guard(tracer_a);
    pid_t tracer_b = fork();
    ASSERT_GE(tracer_b, 0);
    if (tracer_b == 0) {
        RunSeizeContender(tracee, ready[1], start[0], results[1], command_b[0]);
    }
    ProcessGuard tracer_b_guard(tracer_b);

    pid_t reporter = -1;
    ASSERT_TRUE(ReadExactUntil(ready[0], &reporter, sizeof(reporter)));
    ASSERT_TRUE(ReadExactUntil(ready[0], &reporter, sizeof(reporter)));
    const char go[2] = {'G', 'G'};
    ASSERT_TRUE(WriteExact(start[1], go, sizeof(go)));

    OpResult first[2] = {};
    ASSERT_TRUE(ReadExactUntil(results[0], &first[0], sizeof(first[0])));
    ASSERT_TRUE(ReadExactUntil(results[0], &first[1], sizeof(first[1])));
    const int successes = (first[0].result == 0) + (first[1].result == 0);
    ASSERT_EQ(1, successes);
    const OpResult& loser = first[first[0].result == 0 ? 1 : 0];
    EXPECT_EQ(-1, loser.result);
    EXPECT_EQ(EPERM, loser.error);

    const char detach = 'D';
    ASSERT_TRUE(WriteExact(command_a[1], &detach, sizeof(detach)));
    ASSERT_TRUE(WriteExact(command_b[1], &detach, sizeof(detach)));
    OpResult completed[2] = {};
    ASSERT_TRUE(ReadExactUntil(results[0], &completed[0], sizeof(completed[0])));
    ASSERT_TRUE(ReadExactUntil(results[0], &completed[1], sizeof(completed[1])));
    EXPECT_EQ(0, completed[0].result);
    EXPECT_EQ(0, completed[1].result);

    int status = 0;
    ASSERT_TRUE(WaitPidUntil(tracer_a, &status));
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));
    tracer_a_guard.Release();
    ASSERT_TRUE(WaitPidUntil(tracer_b, &status));
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));
    tracer_b_guard.Release();

    int third_ready[2], third_start[2], third_result[2], third_command[2];
    ASSERT_EQ(0, pipe(third_ready));
    ASSERT_EQ(0, pipe(third_start));
    ASSERT_EQ(0, pipe(third_result));
    ASSERT_EQ(0, pipe(third_command));
    pid_t third = fork();
    ASSERT_GE(third, 0);
    if (third == 0) {
        RunSeizeContender(tracee, third_ready[1], third_start[0],
                          third_result[1], third_command[0]);
    }
    ProcessGuard third_guard(third);
    ASSERT_TRUE(ReadExactUntil(third_ready[0], &reporter, sizeof(reporter)));
    ASSERT_TRUE(WriteExact(third_start[1], &detach, sizeof(detach)));
    OpResult takeover = {};
    ASSERT_TRUE(ReadExactUntil(third_result[0], &takeover, sizeof(takeover)));
    ASSERT_EQ(0, takeover.result);
    ASSERT_TRUE(WriteExact(third_command[1], &detach, sizeof(detach)));
    ASSERT_TRUE(ReadExactUntil(third_result[0], &takeover, sizeof(takeover)));
    ASSERT_EQ(0, takeover.result);
    ASSERT_TRUE(WaitPidUntil(third, &status));
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));
    third_guard.Release();
}

TEST(PtraceRelationLifecycle, TracerExitHonorsExitKillAndStartsCleanSession) {
    pid_t doomed = fork();
    ASSERT_GE(doomed, 0);
    if (doomed == 0) {
        for (;;) {
            pause();
        }
    }
    ProcessGuard doomed_guard(doomed);

    pid_t survivor = fork();
    ASSERT_GE(survivor, 0);
    if (survivor == 0) {
        for (;;) {
            syscall(SYS_getpid);
        }
    }
    ProcessGuard survivor_guard(survivor);

    int ready[2], release[2];
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(release));
    pid_t old_tracer = fork();
    ASSERT_GE(old_tracer, 0);
    if (old_tracer == 0) {
        OpResult setup = {getpid(), 0, 0, 0};
        if (ptrace_call(PTRACE_SEIZE, doomed, 0, PTRACE_O_EXITKILL) != 0 ||
            ptrace_call(PTRACE_SEIZE, survivor, 0, PTRACE_O_TRACESYSGOOD) != 0 ||
            ptrace_call(PTRACE_INTERRUPT, doomed, 0, 0) != 0 ||
            !WaitPidUntil(doomed, &setup.status, __WALL) ||
            ptrace_call(PTRACE_INTERRUPT, survivor, 0, 0) != 0 ||
            !WaitPidUntil(survivor, &setup.status, __WALL)) {
            setup.result = -1;
            setup.error = errno;
        }
        WriteExact(ready[1], &setup, sizeof(setup));
        char token = 0;
        if (setup.result == 0) {
            ReadExactUntil(release[0], &token, sizeof(token));
        }
        _exit(setup.result == 0 ? 0 : 90);
    }
    ProcessGuard old_tracer_guard(old_tracer);
    OpResult setup = {};
    ASSERT_TRUE(ReadExactUntil(ready[0], &setup, sizeof(setup)));
    ASSERT_EQ(0, setup.result) << setup.error;
    const char exit_now = 'X';
    ASSERT_TRUE(WriteExact(release[1], &exit_now, sizeof(exit_now)));

    int status = 0;
    ASSERT_TRUE(WaitPidUntil(old_tracer, &status));
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));
    old_tracer_guard.Release();
    ASSERT_TRUE(WaitPidUntil(doomed, &status));
    ASSERT_TRUE(WIFSIGNALED(status));
    ASSERT_EQ(SIGKILL, WTERMSIG(status));
    doomed_guard.Release();
    ASSERT_EQ(0, kill(survivor, 0));

    int fresh_result[2];
    ASSERT_EQ(0, pipe(fresh_result));
    pid_t fresh_tracer = fork();
    ASSERT_GE(fresh_tracer, 0);
    if (fresh_tracer == 0) {
        OpResult fresh = {getpid(), 0, 0, 0};
        if (ptrace_call(PTRACE_SEIZE, survivor, 0, 0) != 0 ||
            ptrace_call(PTRACE_INTERRUPT, survivor, 0, 0) != 0 ||
            !WaitPidUntil(survivor, &fresh.status, __WALL)) {
            fresh.result = -1;
            fresh.error = errno;
        }
        if (fresh.result == 0 &&
            ptrace_call(PTRACE_SYSCALL, survivor, 0, 0) != 0) {
            fresh.result = -1;
            fresh.error = errno;
        }
        if (fresh.result == 0 &&
            !WaitPidUntil(survivor, &fresh.status, __WALL)) {
            fresh.result = -1;
            fresh.error = errno;
        }
        if (fresh.result == 0 &&
            ptrace_call(PTRACE_DETACH, survivor, 0, 0) != 0) {
            fresh.result = -1;
            fresh.error = errno;
        }
        WriteExact(fresh_result[1], &fresh, sizeof(fresh));
        _exit(fresh.result == 0 ? 0 : 91);
    }
    ProcessGuard fresh_tracer_guard(fresh_tracer);
    OpResult fresh = {};
    ASSERT_TRUE(ReadExactUntil(fresh_result[0], &fresh, sizeof(fresh)));
    ASSERT_EQ(0, fresh.result) << fresh.error;
    ASSERT_TRUE(WIFSTOPPED(fresh.status));
    EXPECT_EQ(SIGTRAP, WSTOPSIG(fresh.status));
    ASSERT_TRUE(WaitPidUntil(fresh_tracer, &status));
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));
    fresh_tracer_guard.Release();
}

TEST(PtraceRelationLifecycle, TracerExitPreservesGroupStop) {
    int progress[2];
    ASSERT_EQ(0, pipe(progress));
    pid_t tracee = fork();
    ASSERT_GE(tracee, 0);
    if (tracee == 0) {
        const char stopped = 'S';
        if (!WriteExact(progress[1], &stopped, sizeof(stopped))) {
            _exit(100);
        }
        raise(SIGSTOP);
        const char continued = 'C';
        if (!WriteExact(progress[1], &continued, sizeof(continued))) {
            _exit(101);
        }
        for (;;) {
            pause();
        }
    }
    ProcessGuard tracee_guard(tracee);
    char marker = 0;
    ASSERT_TRUE(ReadExactUntil(progress[0], &marker, sizeof(marker)));
    ASSERT_EQ('S', marker);
    int status = 0;
    ASSERT_TRUE(WaitPidUntil(tracee, &status, WUNTRACED));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));

    int setup_pipe[2], exit_pipe[2];
    ASSERT_EQ(0, pipe(setup_pipe));
    ASSERT_EQ(0, pipe(exit_pipe));
    pid_t tracer = fork();
    ASSERT_GE(tracer, 0);
    if (tracer == 0) {
        errno = 0;
        OpResult setup = {getpid(), ptrace_call(PTRACE_SEIZE, tracee, 0, 0),
                          0, 0};
        if (setup.result < 0) {
            setup.error = errno;
        }
        WriteExact(setup_pipe[1], &setup, sizeof(setup));
        char token = 0;
        if (setup.result == 0) {
            ReadExactUntil(exit_pipe[0], &token, sizeof(token));
        }
        _exit(setup.result == 0 ? 0 : 102);
    }
    ProcessGuard tracer_guard(tracer);
    OpResult setup = {};
    ASSERT_TRUE(ReadExactUntil(setup_pipe[0], &setup, sizeof(setup)));
    ASSERT_EQ(0, setup.result) << setup.error;
    const char exit_now = 'X';
    ASSERT_TRUE(WriteExact(exit_pipe[1], &exit_now, sizeof(exit_now)));
    ASSERT_TRUE(WaitPidUntil(tracer, &status));
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));
    tracer_guard.Release();

    errno = 0;
    EXPECT_FALSE(ReadExactUntil(progress[0], &marker, sizeof(marker),
                                std::chrono::milliseconds(100)));
    EXPECT_EQ(ETIMEDOUT, errno);
    ASSERT_EQ(0, kill(tracee, SIGCONT));
    ASSERT_TRUE(ReadExactUntil(progress[0], &marker, sizeof(marker)));
    ASSERT_EQ('C', marker);
    ASSERT_TRUE(WaitPidUntil(tracee, &status, WCONTINUED));
    ASSERT_TRUE(WIFCONTINUED(status));
}

struct WaiterResult {
    pid_t result;
    int error;
    int status;
};

struct WaiterArgs {
    pid_t tracee;
    pthread_barrier_t* start;
    WaiterResult* result;
};

void* WaitForTracedZombie(void* opaque) {
    auto* args = static_cast<WaiterArgs*>(opaque);
    pthread_barrier_wait(args->start);
    const auto deadline = std::chrono::steady_clock::now() +
                          std::chrono::seconds(3);
    for (;;) {
        errno = 0;
        const pid_t result = waitpid(args->tracee, &args->result->status,
                                     __WALL | WNOHANG);
        if (result == args->tracee || (result < 0 && errno != EINTR)) {
            args->result->result = result;
            args->result->error = result < 0 ? errno : 0;
            return nullptr;
        }
        if (std::chrono::steady_clock::now() >= deadline) {
            args->result->result = -1;
            args->result->error = ETIMEDOUT;
            return nullptr;
        }
        sched_yield();
    }
}

TEST(PtraceRelationLifecycle, ConcurrentTracerWaitersCascadeZombieExactlyOnce) {
    int tracee_exit[2];
    ASSERT_EQ(0, pipe(tracee_exit));
    pid_t tracee = fork();
    ASSERT_GE(tracee, 0);
    if (tracee == 0) {
        char token = 0;
        if (!ReadExactUntil(tracee_exit[0], &token, sizeof(token))) {
            _exit(110);
        }
        _exit(44);
    }
    ProcessGuard tracee_guard(tracee);

    int setup_pipe[2], waiters_ready[2], waiter_results[2], release[2];
    ASSERT_EQ(0, pipe(setup_pipe));
    ASSERT_EQ(0, pipe(waiters_ready));
    ASSERT_EQ(0, pipe(waiter_results));
    ASSERT_EQ(0, pipe(release));
    pid_t tracer = fork();
    ASSERT_GE(tracer, 0);
    if (tracer == 0) {
        errno = 0;
        OpResult setup = {getpid(), ptrace_call(PTRACE_SEIZE, tracee, 0, 0),
                          0, 0};
        if (setup.result < 0) {
            setup.error = errno;
        }
        WriteExact(setup_pipe[1], &setup, sizeof(setup));
        if (setup.result < 0) {
            _exit(111);
        }

        pthread_barrier_t barrier;
        if (pthread_barrier_init(&barrier, nullptr, 3) != 0) {
            _exit(112);
        }
        WaiterResult results[2] = {};
        WaiterArgs args[2] = {{tracee, &barrier, &results[0]},
                              {tracee, &barrier, &results[1]}};
        pthread_t waiters[2];
        if (pthread_create(&waiters[0], nullptr, WaitForTracedZombie,
                           &args[0]) != 0 ||
            pthread_create(&waiters[1], nullptr, WaitForTracedZombie,
                           &args[1]) != 0) {
            _exit(113);
        }
        pthread_barrier_wait(&barrier);
        const char ready = 'R';
        WriteExact(waiters_ready[1], &ready, sizeof(ready));
        pthread_join(waiters[0], nullptr);
        pthread_join(waiters[1], nullptr);
        pthread_barrier_destroy(&barrier);
        WriteExact(waiter_results[1], results, sizeof(results));
        char token = 0;
        ReadExactUntil(release[0], &token, sizeof(token));
        _exit(0);
    }
    ProcessGuard tracer_guard(tracer);
    OpResult setup = {};
    ASSERT_TRUE(ReadExactUntil(setup_pipe[0], &setup, sizeof(setup)));
    ASSERT_EQ(0, setup.result) << setup.error;
    char token = 0;
    ASSERT_TRUE(ReadExactUntil(waiters_ready[0], &token, sizeof(token)));
    ASSERT_EQ('R', token);
    const char exit_now = 'X';
    ASSERT_TRUE(WriteExact(tracee_exit[1], &exit_now, sizeof(exit_now)));

    WaiterResult results[2] = {};
    ASSERT_TRUE(ReadExactUntil(waiter_results[0], results, sizeof(results)));
    const int winners = (results[0].result == tracee) +
                        (results[1].result == tracee);
    ASSERT_EQ(1, winners);
    const WaiterResult& winner =
        results[results[0].result == tracee ? 0 : 1];
    const WaiterResult& loser =
        results[results[0].result == tracee ? 1 : 0];
    ASSERT_TRUE(WIFEXITED(winner.status));
    ASSERT_EQ(44, WEXITSTATUS(winner.status));
    ASSERT_EQ(-1, loser.result);
    ASSERT_EQ(ECHILD, loser.error);

    int natural_status = 0;
    ASSERT_TRUE(WaitPidUntil(tracee, &natural_status));
    ASSERT_TRUE(WIFEXITED(natural_status));
    ASSERT_EQ(44, WEXITSTATUS(natural_status));
    tracee_guard.Release();
    errno = 0;
    ASSERT_EQ(-1, waitpid(tracee, &natural_status, WNOHANG));
    ASSERT_EQ(ECHILD, errno);

    ASSERT_TRUE(WriteExact(release[1], &exit_now, sizeof(exit_now)));
    ASSERT_TRUE(WaitPidUntil(tracer, &natural_status));
    ASSERT_TRUE(WIFEXITED(natural_status));
    ASSERT_EQ(0, WEXITSTATUS(natural_status));
    tracer_guard.Release();
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
