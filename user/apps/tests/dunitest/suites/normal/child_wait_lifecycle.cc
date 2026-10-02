#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <atomic>
#include <cstdio>
#include <cstdlib>
#include <string>

#include "gtest/gtest.h"

#ifndef __WNOTHREAD
#define __WNOTHREAD 0x20000000
#endif

namespace {

enum class Selector { Pid, Any, Group };
struct Scenario {
    pthread_t leader;
    Selector selector;
    bool interrupt;
    bool sibling;
};

long long Milliseconds(clockid_t clock = CLOCK_MONOTONIC) {
    timespec now{};
    if (clock_gettime(clock, &now)) _exit(90);
    return static_cast<long long>(now.tv_sec) * 1000 + now.tv_nsec / 1000000;
}

void SleepMs(unsigned ms) {
    timespec delay{static_cast<time_t>(ms / 1000),
                   static_cast<long>(ms % 1000) * 1000000};
    while (nanosleep(&delay, &delay) && errno == EINTR) {}
}

volatile sig_atomic_t handled = 0;
void HandleSignal(int) { handled = 1; }

struct SignalSender {
    pthread_t waiter;
    std::atomic<bool> finished{false};
};

void* InterruptWaiter(void* argument) {
    auto* sender = static_cast<SignalSender*>(argument);
    // Repeat until wait returns: a signal racing just before wait entry must
    // not make this regression test depend on host scheduling latency.
    while (!sender->finished.load(std::memory_order_acquire)) {
        SleepMs(100);
        if (!sender->finished.load(std::memory_order_acquire) &&
            pthread_kill(sender->waiter, SIGUSR1)) _exit(91);
    }
    return nullptr;
}

int RunWait(Selector selector, bool interrupt) {
    struct sigaction action{};
    action.sa_handler = HandleSignal;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR1, &action, nullptr)) return 10;
    sigset_t unblock;
    sigemptyset(&unblock);
    sigaddset(&unblock, SIGUSR1);
    if (pthread_sigmask(SIG_UNBLOCK, &unblock, nullptr)) return 11;

    const pid_t child = fork();
    if (child < 0) return 12;
    if (!child) {
        SleepMs(interrupt ? 800 : 150);
        _exit(37);
    }
    const pid_t requested = selector == Selector::Pid ? child :
                            selector == Selector::Any ? -1 : -getpgrp();
    SignalSender signal_sender{pthread_self()};
    pthread_t sender{};
    if (interrupt && pthread_create(&sender, nullptr, InterruptWaiter, &signal_sender)) return 13;
    int status = 0;
    const long long start = Milliseconds();
    const long long cpu_start = Milliseconds(CLOCK_THREAD_CPUTIME_ID);
    errno = 0;
    const pid_t result = waitpid(requested, &status, 0);
    const int error = errno;
    signal_sender.finished.store(true, std::memory_order_release);
    const long long elapsed = Milliseconds() - start;
    std::fprintf(stderr, "selector=%d interrupt=%d wait=%d errno=%d wall=%lldms cpu=%lldms\n",
                 static_cast<int>(selector), interrupt, result, error, elapsed,
                 Milliseconds(CLOCK_THREAD_CPUTIME_ID) - cpu_start);
    if (interrupt) {
        if (pthread_join(sender, nullptr)) return 14;
        if (result != -1 || error != EINTR || !handled) return 15;
        // Reap the bounded child after the signal-interrupted wait.
        do { errno = 0; status = 0; } while (waitpid(child, &status, 0) < 0 && errno == EINTR);
        if (!WIFEXITED(status) || WEXITSTATUS(status) != 37) return 16;
    } else if (result != child || !WIFEXITED(status) || WEXITSTATUS(status) != 37) {
        return 17;
    }
    return 0;
}

struct SiblingChild {
    std::atomic<pid_t> child{-1};
    std::atomic<bool> release{false};
    std::atomic<int> result{20};
};

void* OwnSiblingChild(void* argument) {
    auto* state = static_cast<SiblingChild*>(argument);
    int gate[2];
    if (pipe(gate)) _exit(21);
    const pid_t child = fork();
    if (child < 0) _exit(22);
    if (!child) {
        close(gate[1]);
        char byte;
        ssize_t count;
        do { count = read(gate[0], &byte, 1); } while (count < 0 && errno == EINTR);
        _exit(count == 1 ? 38 : 23);
    }
    close(gate[0]);
    state->child.store(child, std::memory_order_release);
    while (!state->release.load(std::memory_order_acquire)) SleepMs(1);
    if (write(gate[1], "x", 1) != 1) _exit(24);
    close(gate[1]);
    int status = 0;
    const pid_t reaped = waitpid(child, &status, __WNOTHREAD);
    state->result.store(reaped == child && WIFEXITED(status) && WEXITSTATUS(status) == 38
                            ? 0 : 25, std::memory_order_release);
    return nullptr;
}

int RunSiblingIsolation() {
    SiblingChild state;
    pthread_t sibling;
    if (pthread_create(&sibling, nullptr, OwnSiblingChild, &state)) return 26;
    pid_t child;
    while ((child = state.child.load(std::memory_order_acquire)) < 0) SleepMs(1);
    int status = 0;
    errno = 0;
    const pid_t isolated = waitpid(child, &status, __WNOTHREAD | WNOHANG);
    const int isolated_error = errno;
    // Without __WNOTHREAD the sibling's live child is eligible, but not ready.
    const pid_t shared = waitpid(child, &status, WNOHANG);
    state.release.store(true, std::memory_order_release);
    if (pthread_join(sibling, nullptr)) return 27;
    if (isolated != -1 || isolated_error != ECHILD || shared != 0) return 28;
    return state.result.load(std::memory_order_acquire);
}

void* SurvivingWorker(void* argument) {
    auto* scenario = static_cast<Scenario*>(argument);
    // Joining the actual initial thread proves it has completed pthread_exit;
    // no sleep is used to infer that the leader's PCB queue was closed.
    if (pthread_join(scenario->leader, nullptr)) _exit(30);
    const int result = scenario->sibling ? RunSiblingIsolation()
                                       : RunWait(scenario->selector, scenario->interrupt);
    _exit(result);
}

void CheckScenario(Selector selector, bool interrupt, bool sibling = false) {
    const pid_t process = fork();
    ASSERT_GE(process, 0);
    if (!process) {
        if (setpgid(0, 0)) _exit(31);
        char selected[] = {static_cast<char>('0' + static_cast<int>(selector)), '\0'};
        // pthread_exit must not unwind through GTest's exception catcher.
        // Enter a normal main() before creating and joining the initial thread.
        execl("/proc/self/exe", "child_wait_lifecycle_test", "--scenario", selected,
              interrupt ? "1" : "0", sibling ? "1" : "0", nullptr);
        _exit(32);
    }
    // Exact scenario-owned process group: timeout cleanup also reaches any
    // child blocked in a pipe. No unrelated process group can be killed.
    if (setpgid(process, process) && errno != EACCES && errno != ESRCH) {
        kill(process, SIGKILL);
    }
    const long long deadline = Milliseconds() + 10000;
    int status = 0;
    pid_t reaped = 0;
    while (Milliseconds() < deadline) {
        reaped = waitpid(process, &status, WNOHANG);
        if (reaped == process || (reaped < 0 && errno != EINTR)) break;
        SleepMs(10);
    }
    if (reaped != process) {
        kill(-process, SIGKILL);
        kill(process, SIGKILL);
        const long long cleanup_deadline = Milliseconds() + 2000;
        while (Milliseconds() < cleanup_deadline) {
            reaped = waitpid(process, &status, WNOHANG);
            if (reaped == process || (reaped < 0 && errno != EINTR)) break;
            SleepMs(10);
        }
        FAIL() << "Scenario did not finish within its independent watchdog";
    }
    kill(-process, SIGKILL);  // Remove scenario-owned descendants on early failure.
    ASSERT_EQ(reaped, process);
    ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

TEST(ChildWaitLifecycle, PidWaitInterruptedAfterLeaderExit) { CheckScenario(Selector::Pid, true); }
TEST(ChildWaitLifecycle, AnyWaitInterruptedAfterLeaderExit) { CheckScenario(Selector::Any, true); }
TEST(ChildWaitLifecycle, GroupWaitInterruptedAfterLeaderExit) { CheckScenario(Selector::Group, true); }
TEST(ChildWaitLifecycle, PidExitWakesSurvivingWorker) { CheckScenario(Selector::Pid, false); }
TEST(ChildWaitLifecycle, AnyExitWakesSurvivingWorker) { CheckScenario(Selector::Any, false); }
TEST(ChildWaitLifecycle, GroupExitWakesSurvivingWorker) { CheckScenario(Selector::Group, false); }
TEST(ChildWaitLifecycle, NotThreadKeepsSiblingChildOwnership) {
    CheckScenario(Selector::Pid, false, true);
}

}  // namespace

int main(int argc, char** argv) {
    if (argc == 5 && std::string(argv[1]) == "--scenario") {
        auto* scenario = new Scenario{pthread_self(), static_cast<Selector>(std::atoi(argv[2])),
                                      std::atoi(argv[3]) != 0, std::atoi(argv[4]) != 0};
        pthread_t worker;
        if (pthread_create(&worker, nullptr, SurvivingWorker, scenario)) _exit(33);
        pthread_exit(nullptr);
    }
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
