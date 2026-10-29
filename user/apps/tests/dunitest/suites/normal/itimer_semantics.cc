#include <gtest/gtest.h>

#include <atomic>
#include <cerrno>
#include <climits>
#include <cstdio>
#include <cstring>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

namespace {

const char* executable;
#define CHECK_CHILD(expression) do { \
    if (!(expression)) { \
        fprintf(stderr, "itimer child line %d: %s (errno=%d)\n", __LINE__, #expression, errno); \
        return 1; \
    } \
} while (0)

bool SleepMs(long ms) {
    timespec delay{ms / 1000, ms % 1000 * 1000000};
    while (nanosleep(&delay, &delay) < 0) if (errno != EINTR) return false;
    return true;
}

sigset_t Mask(int signal) {
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, signal);
    return mask;
}

bool Block(int signal) {
    auto mask = Mask(signal);
    return pthread_sigmask(SIG_BLOCK, &mask, nullptr) == 0;
}

int WaitSignal(int signal, siginfo_t* info = nullptr, long timeout_ms = 2000) {
    auto mask = Mask(signal);
    timespec timeout{timeout_ms / 1000, timeout_ms % 1000 * 1000000};
    siginfo_t ignored{};
    return sigtimedwait(&mask, info ? info : &ignored, &timeout);
}

itimerval Configuration(long usec, long interval_usec = 0) {
    itimerval timer{};
    timer.it_value = {usec / 1000000, usec % 1000000};
    timer.it_interval = {interval_usec / 1000000, interval_usec % 1000000};
    return timer;
}

bool Set(int which, long usec, long interval_usec = 0) {
    auto timer = Configuration(usec, interval_usec);
    return setitimer(which, &timer, nullptr) == 0;
}

bool IsZero(const timeval& value) { return value.tv_sec == 0 && value.tv_usec == 0; }

enum class Scenario {
    RealIdentity, PeriodicWait, PeriodicFd, AlarmInterop, ThreadShared,
    ForkClear, ExecKeep, LeaderExit, VirtualGroup, ProfGroup, NullAndFault,
    IntervalDisabled, WideValue, Namespace, ResetCancel, HandlerPeriodic,
    SighandSeparate, SighandIdentity, SighandGroupExit
};

volatile sig_atomic_t handler_deliveries = 0;
void AlarmHandler(int) { ++handler_deliveries; }

int SeparatePendingChild(void*) {
    siginfo_t info{};
    // The parent deliberately leaves its SIGALRM pending while this distinct
    // thread group waits. Sharing dispositions must not share pending signals.
    int result = WaitSignal(SIGALRM, &info, 150);
    // Isolate pending/TGID ownership from the independent exit_group contract.
    syscall(SYS_exit, result == -1 && errno == EAGAIN ? 0 : 82);
    __builtin_unreachable();
}

int IdentityChild(void*) {
    SleepMs(150);
    syscall(SYS_exit, 0);
    __builtin_unreachable();
}

int GroupExitChild(void*) { _exit(7); }

void* SetAndExit(void*) {
    return reinterpret_cast<void*>(static_cast<intptr_t>(Set(ITIMER_REAL, 200000) ? 0 : 1));
}

void* LeaderSurvivor(void*) {
    siginfo_t info{};
    if (WaitSignal(SIGALRM, &info) != SIGALRM || info.si_code != SI_KERNEL) _exit(71);
    _exit(0);
}

std::atomic<bool> stop_worker{false};
void* BurnCpu(void*) {
    volatile unsigned long accumulator = 1;
    while (!stop_worker.load(std::memory_order_relaxed)) {
        for (int i = 0; i < 10000; ++i) accumulator = accumulator * 1664525UL + 1013904223UL;
    }
    return nullptr;
}

int RunScenario(Scenario scenario) {
    CHECK_CHILD(Block(SIGALRM));
    CHECK_CHILD(Block(SIGVTALRM));
    CHECK_CHILD(Block(SIGPROF));
    siginfo_t info{};
    itimerval observed{};
    switch (scenario) {
        case Scenario::RealIdentity:
            CHECK_CHILD(Set(ITIMER_REAL, 50000));
            CHECK_CHILD(WaitSignal(SIGALRM, &info) == SIGALRM);
            CHECK_CHILD(info.si_code == SI_KERNEL);
            CHECK_CHILD(getitimer(ITIMER_REAL, &observed) == 0);
            CHECK_CHILD(IsZero(observed.it_value));
            return 0;
        case Scenario::PeriodicWait:
        case Scenario::PeriodicFd: {
            int fd = -1;
            if (scenario == Scenario::PeriodicFd) {
                auto mask = Mask(SIGALRM);
                fd = signalfd(-1, &mask, SFD_CLOEXEC | SFD_NONBLOCK);
                CHECK_CHILD(fd >= 0);
            }
            CHECK_CHILD(Set(ITIMER_REAL, 30000, 50000));
            CHECK_CHILD(SleepMs(220));
            CHECK_CHILD(getitimer(ITIMER_REAL, &observed) == 0);
            CHECK_CHILD(IsZero(observed.it_value));
            CHECK_CHILD(observed.it_interval.tv_usec == 50000);
            for (int i = 0; i < 3; ++i) {
                if (fd < 0) {
                    CHECK_CHILD(WaitSignal(SIGALRM, &info) == SIGALRM);
                    CHECK_CHILD(info.si_code == SI_KERNEL);
                } else {
                    pollfd readiness{fd, POLLIN, 0};
                    CHECK_CHILD(poll(&readiness, 1, 2000) == 1);
                    signalfd_siginfo record{};
                    CHECK_CHILD(read(fd, &record, sizeof(record)) == sizeof(record));
                    CHECK_CHILD(record.ssi_signo == SIGALRM && record.ssi_code == SI_KERNEL);
                }
            }
            CHECK_CHILD(Set(ITIMER_REAL, 0));
            if (fd >= 0) close(fd);
            return 0;
        }
        case Scenario::AlarmInterop:
            CHECK_CHILD(Set(ITIMER_REAL, 200000, 50000));
            CHECK_CHILD(alarm(0) == 1);
            CHECK_CHILD(getitimer(ITIMER_REAL, &observed) == 0);
            CHECK_CHILD(IsZero(observed.it_value) && IsZero(observed.it_interval));
            CHECK_CHILD(alarm(3) == 0);
            CHECK_CHILD(Set(ITIMER_REAL, 50000));
            CHECK_CHILD(WaitSignal(SIGALRM, &info) == SIGALRM);
            CHECK_CHILD(info.si_code == SI_KERNEL);
            return 0;
        case Scenario::ThreadShared: {
            pthread_t worker;
            CHECK_CHILD(pthread_create(&worker, nullptr, SetAndExit, nullptr) == 0);
            void* result;
            CHECK_CHILD(pthread_join(worker, &result) == 0 && result == nullptr);
            CHECK_CHILD(getitimer(ITIMER_REAL, &observed) == 0);
            CHECK_CHILD(!IsZero(observed.it_value));
            CHECK_CHILD(WaitSignal(SIGALRM, &info) == SIGALRM);
            CHECK_CHILD(info.si_code == SI_KERNEL);
            return 0;
        }
        case Scenario::ForkClear: {
            CHECK_CHILD(Set(ITIMER_REAL, 5000000));
            pid_t child = fork();
            CHECK_CHILD(child >= 0);
            if (child == 0) {
                if (getitimer(ITIMER_REAL, &observed) != 0 || !IsZero(observed.it_value)) _exit(72);
                _exit(0);
            }
            int status;
            CHECK_CHILD(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
            CHECK_CHILD(getitimer(ITIMER_REAL, &observed) == 0 && !IsZero(observed.it_value));
            return 0;
        }
        case Scenario::ExecKeep:
            CHECK_CHILD(Set(ITIMER_REAL, 200000));
            execl(executable, executable, "--itimer-exec-helper", nullptr);
            return 73;
        case Scenario::LeaderExit: {
            // pthread_exit unwinds the initial thread. Run outside GTest's
            // catch-all exception boundary, which cannot consume forced unwind.
            execl(executable, executable, "--itimer-leader-helper", nullptr);
            return 74;
        }
        case Scenario::VirtualGroup:
        case Scenario::ProfGroup: {
            int which = scenario == Scenario::VirtualGroup ? ITIMER_VIRTUAL : ITIMER_PROF;
            int signal = scenario == Scenario::VirtualGroup ? SIGVTALRM : SIGPROF;
            CHECK_CHILD(Set(which, 60000, 40000));
            CHECK_CHILD(SleepMs(100));
            CHECK_CHILD(getitimer(which, &observed) == 0 && !IsZero(observed.it_value));
            CHECK_CHILD(WaitSignal(signal, nullptr, 0) == -1 && errno == EAGAIN);
            pthread_t worker;
            CHECK_CHILD(pthread_create(&worker, nullptr, BurnCpu, nullptr) == 0);
            bool correct = true;
            for (int i = 0; i < 3; ++i) {
                int delivered = WaitSignal(signal, &info, 5000);
                correct = correct && delivered == signal && info.si_code == SI_KERNEL;
            }
            stop_worker.store(true, std::memory_order_relaxed);
            CHECK_CHILD(pthread_join(worker, nullptr) == 0);
            CHECK_CHILD(correct);
            return 0;
        }
        case Scenario::NullAndFault: {
            CHECK_CHILD(Set(ITIMER_REAL, 5000000));
            CHECK_CHILD(syscall(SYS_setitimer, ITIMER_REAL, nullptr, nullptr) == 0);
            CHECK_CHILD(getitimer(ITIMER_REAL, &observed) == 0 && IsZero(observed.it_value));
            void* inaccessible = mmap(nullptr, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            CHECK_CHILD(inaccessible != MAP_FAILED);
            auto config = Configuration(5000000);
            errno = 0;
            CHECK_CHILD(syscall(SYS_setitimer, ITIMER_REAL, &config, inaccessible) == -1 && errno == EFAULT);
            CHECK_CHILD(getitimer(ITIMER_REAL, &observed) == 0 && !IsZero(observed.it_value));
            CHECK_CHILD(munmap(inaccessible, 4096) == 0);
            return 0;
        }
        case Scenario::IntervalDisabled:
            for (int which : {ITIMER_REAL, ITIMER_VIRTUAL, ITIMER_PROF}) {
                CHECK_CHILD(Set(which, 0, 120000));
                CHECK_CHILD(getitimer(which, &observed) == 0 && IsZero(observed.it_value));
                CHECK_CHILD(observed.it_interval.tv_usec == (which == ITIMER_REAL ? 0 : 120000));
            }
            return 0;
        case Scenario::WideValue: {
            auto config = Configuration(0);
            config.it_value.tv_sec = LONG_MAX;
            for (int which : {ITIMER_REAL, ITIMER_VIRTUAL, ITIMER_PROF}) {
                CHECK_CHILD(setitimer(which, &config, nullptr) == 0);
                CHECK_CHILD(getitimer(which, &observed) == 0 && !IsZero(observed.it_value));
                CHECK_CHILD(Set(which, 0));
            }
            return 0;
        }
        case Scenario::Namespace: {
            if (unshare(CLONE_NEWPID) != 0) return errno == EPERM ? 77 : 75;
            pid_t child = fork();
            CHECK_CHILD(child >= 0);
            if (child == 0) {
                if (!Set(ITIMER_REAL, 50000) || WaitSignal(SIGALRM, &info) != SIGALRM || info.si_code != SI_KERNEL) _exit(76);
                _exit(0);
            }
            int status;
            CHECK_CHILD(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
            return 0;
        }
        case Scenario::ResetCancel:
            for (int i = 0; i < 64; ++i) {
                CHECK_CHILD(Set(ITIMER_REAL, 1000, 1000));
                CHECK_CHILD(Set(ITIMER_REAL, 5000000));
                CHECK_CHILD(Set(ITIMER_REAL, 0));
                // A callback that already committed may leave one pending
                // signal: cancellation must not remove that Linux-visible signal.
                while (WaitSignal(SIGALRM, nullptr, 0) == SIGALRM) {}
            }
            CHECK_CHILD(SleepMs(50));
            CHECK_CHILD(getitimer(ITIMER_REAL, &observed) == 0 && IsZero(observed.it_value));
            // No stale periodic callback may rearm the cancelled timer.
            return 0;
        case Scenario::HandlerPeriodic: {
            struct sigaction action{};
            action.sa_handler = AlarmHandler;
            sigemptyset(&action.sa_mask);
            CHECK_CHILD(sigaction(SIGALRM, &action, nullptr) == 0);
            auto mask = Mask(SIGALRM);
            CHECK_CHILD(pthread_sigmask(SIG_UNBLOCK, &mask, nullptr) == 0);
            CHECK_CHILD(Set(ITIMER_REAL, 30000, 50000));
            CHECK_CHILD(SleepMs(300));
            CHECK_CHILD(Set(ITIMER_REAL, 0));
            CHECK_CHILD(handler_deliveries >= 2);
            return 0;
        }
        case Scenario::SighandSeparate: {
            constexpr size_t stack_size = 128 * 1024;
            void* stack = mmap(nullptr, stack_size, PROT_READ | PROT_WRITE,
                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            CHECK_CHILD(stack != MAP_FAILED);
            pid_t child = clone(SeparatePendingChild, static_cast<char*>(stack) + stack_size,
                                CLONE_VM | CLONE_SIGHAND | SIGCHLD, nullptr);
            CHECK_CHILD(child >= 0);
            CHECK_CHILD(Set(ITIMER_REAL, 30000));
            int status;
            CHECK_CHILD(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
            CHECK_CHILD(WaitSignal(SIGALRM, &info) == SIGALRM && info.si_code == SI_KERNEL);
            // Capture a new TGID target after the child has exited and been
            // reaped. An existing timer alone cannot cover this identity path.
            CHECK_CHILD(Set(ITIMER_REAL, 50000));
            CHECK_CHILD(WaitSignal(SIGALRM, &info) == SIGALRM && info.si_code == SI_KERNEL);
            CHECK_CHILD(munmap(stack, stack_size) == 0);
            return 0;
        }
        case Scenario::SighandIdentity:
        case Scenario::SighandGroupExit: {
            constexpr size_t stack_size = 128 * 1024;
            void* stack = mmap(nullptr, stack_size, PROT_READ | PROT_WRITE,
                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            CHECK_CHILD(stack != MAP_FAILED);
            pid_t original = getpid();
            pid_t child = clone(scenario == Scenario::SighandIdentity ? IdentityChild : GroupExitChild,
                                static_cast<char*>(stack) + stack_size,
                                CLONE_VM | CLONE_SIGHAND | SIGCHLD, nullptr);
            CHECK_CHILD(child >= 0);
            if (scenario == Scenario::SighandIdentity) {
                CHECK_CHILD(getpid() == original);
                CHECK_CHILD(Set(ITIMER_REAL, 30000));
                CHECK_CHILD(WaitSignal(SIGALRM, &info) == SIGALRM && info.si_code == SI_KERNEL);
            }
            int status;
            CHECK_CHILD(waitpid(child, &status, 0) == child && WIFEXITED(status));
            CHECK_CHILD(WEXITSTATUS(status) == (scenario == Scenario::SighandIdentity ? 0 : 7));
            if (scenario == Scenario::SighandIdentity) CHECK_CHILD(getpid() == original);
            CHECK_CHILD(munmap(stack, stack_size) == 0);
            if (scenario == Scenario::SighandGroupExit) {
                // The outer watchdog must observe 42, not the child's 7.
                _exit(42);
            }
            return 0;
        }
    }
    return 78;
}

class ItimerSemantics : public testing::Test {
  protected:
    void TearDown() override {
        if (child_ > 0) {
            kill(child_, SIGKILL);
            while (waitpid(child_, nullptr, 0) < 0 && errno == EINTR) {}
        }
    }

    void Check(Scenario scenario) {
        timespec start{};
        ASSERT_EQ(clock_gettime(CLOCK_MONOTONIC, &start), 0);
        child_ = fork();
        ASSERT_GE(child_, 0);
        if (child_ == 0) _exit(RunScenario(scenario));
        for (;;) {
            int status = 0;
            pid_t result = waitpid(child_, &status, WNOHANG);
            if (result == child_) {
                child_ = -1;
                ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
                if (WEXITSTATUS(status) == 77 && scenario == Scenario::Namespace) GTEST_SKIP() << "PID namespace requires CAP_SYS_ADMIN";
                ASSERT_EQ(WEXITSTATUS(status), scenario == Scenario::SighandGroupExit ? 42 : 0)
                    << "scenario=" << static_cast<int>(scenario);
                return;
            }
            ASSERT_TRUE(result == 0 || (result < 0 && errno == EINTR));
            timespec now{};
            ASSERT_EQ(clock_gettime(CLOCK_MONOTONIC, &now), 0);
            ASSERT_LT((now.tv_sec - start.tv_sec) * INT64_C(1000000000) + now.tv_nsec - start.tv_nsec, INT64_C(15000000000));
            ASSERT_TRUE(SleepMs(10));
        }
    }
    pid_t child_ = -1;
};

TEST_F(ItimerSemantics, RealUsesKernelProcessSignal) { Check(Scenario::RealIdentity); }
TEST_F(ItimerSemantics, PeriodicRestartsOnSigtimedwait) { Check(Scenario::PeriodicWait); }
TEST_F(ItimerSemantics, PeriodicRestartsOnSignalfd) { Check(Scenario::PeriodicFd); }
TEST_F(ItimerSemantics, AlarmAndSetitimerShareRealSlot) { Check(Scenario::AlarmInterop); }
TEST_F(ItimerSemantics, TimerSurvivesSettingThreadExit) { Check(Scenario::ThreadShared); }
TEST_F(ItimerSemantics, ForkDoesNotInheritTimer) { Check(Scenario::ForkClear); }
TEST_F(ItimerSemantics, ExecPreservesTimer) { Check(Scenario::ExecKeep); }
TEST_F(ItimerSemantics, LeaderExitDoesNotCancelGroupTimer) { Check(Scenario::LeaderExit); }
TEST_F(ItimerSemantics, VirtualAccountsWorkerCpu) { Check(Scenario::VirtualGroup); }
TEST_F(ItimerSemantics, ProfAccountsWorkerCpu) { Check(Scenario::ProfGroup); }
TEST_F(ItimerSemantics, NullCancelsAndOutputFaultStillCommits) { Check(Scenario::NullAndFault); }
TEST_F(ItimerSemantics, DisabledIntervalMatchesTimerKind) { Check(Scenario::IntervalDisabled); }
TEST_F(ItimerSemantics, LegalWideSecondsDoNotWrap) { Check(Scenario::WideValue); }
TEST_F(ItimerSemantics, TimerWorksInPidNamespace) { Check(Scenario::Namespace); }
TEST_F(ItimerSemantics, ResetAndCancelDoNotResurrectPeriodicTimer) { Check(Scenario::ResetCancel); }
TEST_F(ItimerSemantics, PeriodicRestartsOnHandlerDelivery) { Check(Scenario::HandlerPeriodic); }
TEST_F(ItimerSemantics, CloneSighandDoesNotShareProcessPending) { Check(Scenario::SighandSeparate); }
TEST_F(ItimerSemantics, CloneSighandDoesNotChangeParentIdentity) { Check(Scenario::SighandIdentity); }
TEST_F(ItimerSemantics, CloneSighandDoesNotShareGroupExitCode) { Check(Scenario::SighandGroupExit); }

}  // namespace

int main(int argc, char** argv) {
    executable = argv[0];
    if (argc == 2 && strcmp(argv[1], "--itimer-exec-helper") == 0) {
        siginfo_t info{};
        return WaitSignal(SIGALRM, &info) == SIGALRM && info.si_code == SI_KERNEL ? 0 : 79;
    }
    if (argc == 2 && strcmp(argv[1], "--itimer-leader-helper") == 0) {
        if (!Block(SIGALRM) || !Set(ITIMER_REAL, 200000)) return 80;
        pthread_t worker;
        if (pthread_create(&worker, nullptr, LeaderSurvivor, nullptr) != 0) return 81;
        pthread_exit(nullptr);
    }
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
