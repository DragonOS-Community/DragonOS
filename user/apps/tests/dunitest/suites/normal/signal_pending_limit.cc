#include <gtest/gtest.h>

#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <atomic>
#include <functional>

namespace {

volatile sig_atomic_t timer_deliveries = 0;

void CountAlarm(int) {
    ++timer_deliveries;
}

void IgnoreHandler(int) {}

// Each scenario owns a real UID and uses at most a handful of pending signals.
// Testing as UID 0 directly would count unrelated host/guest processes too.
void Isolated(const std::function<void()>& scenario, bool saved_root = false) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "independent signal-accounting UIDs require root";
    }
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        alarm(15);
        // Install a known ceiling while privileged. Each scenario lowers it
        // after changing UID; its result must not depend on the init default.
        const rlimit initial_limit = {64, 64};
        if (setrlimit(RLIMIT_SIGPENDING, &initial_limit) != 0) {
            _exit(119);
        }
        const uid_t uid = 1000000U + static_cast<uid_t>(getpid());
        if (setresuid(uid, uid, saved_root ? 0 : uid) != 0) {
            _exit(120);
        }
        scenario();
        _exit(testing::Test::HasFailure() ? 1 : 0);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status)) << "child status=" << status;
    if (WEXITSTATUS(status) == 77) {
        GTEST_SKIP() << "host policy forbids creating user namespaces";
    }
    EXPECT_EQ(0, WEXITSTATUS(status));
}

void Limit(rlim_t value) {
    rlimit limit = {value, value};
    ASSERT_EQ(0, setrlimit(RLIMIT_SIGPENDING, &limit));
}

void Block(int signal) {
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, signal);
    ASSERT_EQ(0, pthread_sigmask(SIG_BLOCK, &mask, nullptr));
}

int Receive(int signal, siginfo_t* info) {
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, signal);
    const timespec zero = {};
    return sigtimedwait(&mask, info, &zero);
}

int Queue(pid_t pid, int signal, int value = 42) {
    sigval payload = {};
    payload.sival_int = value;
    return sigqueue(pid, signal, payload);
}

bool ReadSigQ(unsigned long long* used, unsigned long long* limit) {
    FILE* status = fopen("/proc/self/status", "r");
    if (!status) {
        return false;
    }
    char line[1024];
    bool found = false;
    while (fgets(line, sizeof(line), status)) {
        if (sscanf(line, "SigQ: %llu/%llu", used, limit) == 2) {
            found = true;
            break;
        }
    }
    fclose(status);
    return found;
}

void CheckSigQ(unsigned long long expected, unsigned long long limit) {
    unsigned long long used = 0;
    unsigned long long actual_limit = 0;
    ASSERT_TRUE(ReadSigQ(&used, &actual_limit));
    EXPECT_EQ(expected, used);
    EXPECT_EQ(limit, actual_limit);
}

// The parent always kills/reaps on a failed assertion, so no helper survives a
// scenario. Pipe requests avoid sleeps and keep the receiver blocked throughout.
class Receiver {
public:
    Receiver() = default;
    ~Receiver() {
        if (pid_ > 0) {
            kill(pid_, SIGKILL);
            waitpid(pid_, nullptr, 0);
        }
        for (int fd : {command_[0], command_[1], result_[0], result_[1]}) {
            if (fd >= 0) {
                close(fd);
            }
        }
    }

    bool Start(rlim_t limit, bool timer = false) {
        if (pipe(command_) != 0 || pipe(result_) != 0) {
            return false;
        }
        pid_ = fork();
        if (pid_ < 0) {
            return false;
        }
        if (pid_ == 0) {
            alarm(10);
            close(command_[1]);
            close(result_[0]);
            rlimit resource = {limit, limit};
            sigset_t mask;
            sigemptyset(&mask);
            sigaddset(&mask, SIGRTMIN);
            if (setrlimit(RLIMIT_SIGPENDING, &resource) != 0 ||
                sigprocmask(SIG_BLOCK, &mask, nullptr) != 0) {
                _exit(121);
            }
            int id = -1;
            if (timer) {
                sigevent event = {};
                event.sigev_notify = SIGEV_NONE;
                if (syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &id) != 0) {
                    _exit(122);
                }
            }
            char response = 'R';
            if (write(result_[1], &response, 1) != 1) {
                _exit(123);
            }
            char command;
            while (read(command_[0], &command, 1) == 1) {
                if (command == 'Q') {
                    _exit(0);
                }
                siginfo_t info = {};
                const timespec zero = {};
                response = sigtimedwait(&mask, &info, &zero) == SIGRTMIN ? 'D' : 'E';
                if (write(result_[1], &response, 1) != 1) {
                    _exit(124);
                }
            }
            _exit(125);
        }
        close(command_[0]);
        command_[0] = -1;
        close(result_[1]);
        result_[1] = -1;
        char ready = 0;
        return read(result_[0], &ready, 1) == 1 && ready == 'R';
    }

    pid_t pid() const { return pid_; }

    bool DrainOne() {
        const char command = 'D';
        char response = 0;
        return write(command_[1], &command, 1) == 1 &&
               read(result_[0], &response, 1) == 1 && response == 'D';
    }

    bool Exit() {
        const char command = 'Q';
        if (write(command_[1], &command, 1) != 1) {
            return false;
        }
        return Reap();
    }

    bool BecomeZombie() {
        const char command = 'Q';
        if (write(command_[1], &command, 1) != 1) {
            return false;
        }
        siginfo_t info = {};
        return waitid(P_PID, pid_, &info, WEXITED | WNOWAIT) == 0 &&
               info.si_pid == pid_ && info.si_code == CLD_EXITED;
    }

    bool Reap() {
        int status = 0;
        if (waitpid(pid_, &status, 0) != pid_) {
            return false;
        }
        pid_ = -1;
        return WIFEXITED(status) && WEXITSTATUS(status) == 0;
    }

private:
    pid_t pid_ = -1;
    int command_[2] = {-1, -1};
    int result_[2] = {-1, -1};
};

}  // namespace

TEST(SignalPendingLimit, DefaultLimitIsUsable) {
    rlimit limit = {};
    ASSERT_EQ(0, getrlimit(RLIMIT_SIGPENDING, &limit));
    EXPECT_GT(limit.rlim_cur, 0U);
    EXPECT_GE(limit.rlim_max, limit.rlim_cur);
}

TEST(SignalPendingLimit, RealtimeQueueDrainsAndReusesCapacity) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(2);
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN, 10));
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN, 20));
        errno = 0;
        EXPECT_EQ(-1, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
        siginfo_t info = {};
        ASSERT_EQ(SIGRTMIN, Receive(SIGRTMIN, &info));
        EXPECT_EQ(SI_QUEUE, info.si_code);
        EXPECT_EQ(10, info.si_value.sival_int);
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN, 30));
        ASSERT_EQ(SIGRTMIN, Receive(SIGRTMIN, &info));
        EXPECT_EQ(20, info.si_value.sival_int);
        ASSERT_EQ(SIGRTMIN, Receive(SIGRTMIN, &info));
        EXPECT_EQ(30, info.si_value.sival_int);
    });
}

TEST(SignalPendingLimit, TargetLimitNotSenderLimit) {
    Isolated([] {
        Receiver receiver;
        ASSERT_TRUE(receiver.Start(2));
        Limit(0);
        ASSERT_EQ(0, Queue(receiver.pid(), SIGRTMIN));
        ASSERT_EQ(0, Queue(receiver.pid(), SIGRTMIN));
        errno = 0;
        EXPECT_EQ(-1, Queue(receiver.pid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
        ASSERT_TRUE(receiver.DrainOne());
        EXPECT_EQ(0, Queue(receiver.pid(), SIGRTMIN));
    });
}

TEST(SignalPendingLimit, ProcessesShareRealUidInventory) {
    Isolated([] {
        Receiver first;
        Receiver second;
        ASSERT_TRUE(first.Start(2));
        ASSERT_TRUE(second.Start(2));
        ASSERT_EQ(0, Queue(first.pid(), SIGRTMIN));
        ASSERT_EQ(0, Queue(second.pid(), SIGRTMIN));
        errno = 0;
        EXPECT_EQ(-1, Queue(second.pid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
        ASSERT_TRUE(first.DrainOne());
        EXPECT_EQ(0, Queue(second.pid(), SIGRTMIN));
    });
}

TEST(SignalPendingLimit, EachTargetAppliesItsOwnLimitToSharedInventory) {
    Isolated([] {
        Receiver first;
        Receiver second;
        ASSERT_TRUE(first.Start(2));
        ASSERT_TRUE(second.Start(1));
        ASSERT_EQ(0, Queue(first.pid(), SIGRTMIN));
        errno = 0;
        EXPECT_EQ(-1, Queue(second.pid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
        ASSERT_TRUE(first.DrainOne());
        EXPECT_EQ(0, Queue(second.pid(), SIGRTMIN));
    });
}

TEST(SignalPendingLimit, ThreadAndSharedPendingUseSameInventory) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(2);
        std::atomic<pid_t> tid{0};
        std::atomic<bool> drain{false};
        std::atomic<int> received{0};
        struct Context {
            std::atomic<pid_t>* tid;
            std::atomic<bool>* drain;
            std::atomic<int>* received;
        } context{&tid, &drain, &received};
        pthread_t thread;
        ASSERT_EQ(0, pthread_create(&thread, nullptr, [](void* arg) -> void* {
            auto* context = static_cast<Context*>(arg);
            context->tid->store(syscall(SYS_gettid));
            while (!context->drain->load()) {
                sched_yield();
            }
            siginfo_t info = {};
            context->received->store(Receive(SIGRTMIN, &info));
            return nullptr;
        }, &context));
        while (tid.load() == 0) {
            sched_yield();
        }
        siginfo_t info = {};
        info.si_signo = SIGRTMIN;
        info.si_code = SI_QUEUE;
        info.si_pid = getpid();
        info.si_uid = getuid();
        EXPECT_EQ(0, syscall(SYS_rt_tgsigqueueinfo, getpid(), tid.load(),
                             SIGRTMIN, &info));
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN));
        errno = 0;
        EXPECT_EQ(-1, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
        drain.store(true);
        EXPECT_EQ(0, pthread_join(thread, nullptr));
        EXPECT_EQ(SIGRTMIN, received.load());
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN));
    });
}

TEST(SignalPendingLimit, StandardSignalsCoalesceAndKillBypassesLimit) {
    Isolated([] {
        Block(SIGUSR1);
        Block(SIGRTMIN);
        Limit(1);
        ASSERT_EQ(0, Queue(getpid(), SIGUSR1, 10));
        ASSERT_EQ(0, Queue(getpid(), SIGUSR1, 20));
        errno = 0;
        EXPECT_EQ(-1, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
        siginfo_t info = {};
        ASSERT_EQ(SIGUSR1, Receive(SIGUSR1, &info));
        EXPECT_EQ(10, info.si_value.sival_int);
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN));
        ASSERT_EQ(0, kill(getpid(), SIGUSR1));
        ASSERT_EQ(0, kill(getpid(), SIGUSR1));
        ASSERT_EQ(SIGUSR1, Receive(SIGUSR1, &info));
        EXPECT_EQ(SI_USER, info.si_code);
        errno = 0;
        EXPECT_EQ(-1, Receive(SIGUSR1, &info));
        EXPECT_EQ(EAGAIN, errno);
        ASSERT_EQ(SIGRTMIN, Receive(SIGRTMIN, &info));
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN));
    });
}

TEST(SignalPendingLimit, ZeroLimitStillDeliversKillAndStandardFallback) {
    Isolated([] {
        Block(SIGRTMIN);
        Block(SIGUSR1);
        Limit(0);
        errno = 0;
        EXPECT_EQ(-1, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
        ASSERT_EQ(0, kill(getpid(), SIGRTMIN));
        ASSERT_EQ(0, kill(getpid(), SIGRTMIN));
        siginfo_t info = {};
        ASSERT_EQ(SIGRTMIN, Receive(SIGRTMIN, &info));
        EXPECT_EQ(SI_USER, info.si_code);
        errno = 0;
        EXPECT_EQ(-1, Receive(SIGRTMIN, &info));
        EXPECT_EQ(EAGAIN, errno);
        ASSERT_EQ(0, Queue(getpid(), SIGUSR1));
        ASSERT_EQ(SIGUSR1, Receive(SIGUSR1, &info));
        EXPECT_EQ(SI_USER, info.si_code);
    });
}

TEST(SignalPendingLimit, ThreadExitReleasesPrivatePendingEntries) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(1);
        std::atomic<pid_t> tid{0};
        std::atomic<bool> exit{false};
        struct Context {
            std::atomic<pid_t>* tid;
            std::atomic<bool>* exit;
        } context{&tid, &exit};
        pthread_t thread;
        ASSERT_EQ(0, pthread_create(&thread, nullptr, [](void* arg) -> void* {
            auto* context = static_cast<Context*>(arg);
            context->tid->store(syscall(SYS_gettid));
            while (!context->exit->load()) {
                sched_yield();
            }
            return nullptr;
        }, &context));
        while (tid.load() == 0) {
            sched_yield();
        }
        siginfo_t info = {};
        info.si_signo = SIGRTMIN;
        info.si_code = SI_QUEUE;
        info.si_pid = getpid();
        info.si_uid = getuid();
        EXPECT_EQ(0, syscall(SYS_rt_tgsigqueueinfo, getpid(), tid.load(),
                             SIGRTMIN, &info));
        errno = 0;
        EXPECT_EQ(-1, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
        exit.store(true);
        EXPECT_EQ(0, pthread_join(thread, nullptr));
        // Linux clears child_tid in exit_mm before __exit_signal frees its
        // private queue. pthread_join is therefore not the refund barrier.
        int queued = -1;
        for (int attempt = 0; attempt < 1000; ++attempt) {
            queued = Queue(getpid(), SIGRTMIN);
            if (queued == 0 || errno != EAGAIN) {
                break;
            }
            usleep(1000);
        }
        EXPECT_EQ(0, queued) << "thread pending inventory was not released";
    });
}

TEST(SignalPendingLimit, IgnoringSignalReleasesItsPendingEntries) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(2);
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN));
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN));
        struct sigaction action = {};
        action.sa_handler = SIG_IGN;
        sigemptyset(&action.sa_mask);
        ASSERT_EQ(0, sigaction(SIGRTMIN, &action, nullptr));
        action.sa_handler = SIG_DFL;
        ASSERT_EQ(0, sigaction(SIGRTMIN, &action, nullptr));
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN));
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN));
        errno = 0;
        EXPECT_EQ(-1, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
    });
}

TEST(SignalPendingLimit, IgnoreClearsSharedAndOtherThreadPrivateEntries) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(2);
        std::atomic<pid_t> tid{0};
        std::atomic<bool> exit{false};
        struct Context {
            std::atomic<pid_t>* tid;
            std::atomic<bool>* exit;
        } context{&tid, &exit};
        pthread_t thread;
        ASSERT_EQ(0, pthread_create(&thread, nullptr, [](void* arg) -> void* {
            auto* context = static_cast<Context*>(arg);
            context->tid->store(syscall(SYS_gettid));
            while (!context->exit->load()) {
                sched_yield();
            }
            return nullptr;
        }, &context));
        while (tid.load() == 0) {
            sched_yield();
        }
        siginfo_t info = {};
        info.si_signo = SIGRTMIN;
        info.si_code = SI_QUEUE;
        info.si_pid = getpid();
        info.si_uid = getuid();
        EXPECT_EQ(0, syscall(SYS_rt_tgsigqueueinfo, getpid(), tid.load(),
                             SIGRTMIN, &info));
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN));
        CheckSigQ(2, 2);
        struct sigaction action = {};
        action.sa_handler = SIG_IGN;
        sigemptyset(&action.sa_mask);
        EXPECT_EQ(0, sigaction(SIGRTMIN, &action, nullptr));
        CheckSigQ(0, 2);
        action.sa_handler = SIG_DFL;
        EXPECT_EQ(0, sigaction(SIGRTMIN, &action, nullptr));
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN));
        exit.store(true);
        EXPECT_EQ(0, pthread_join(thread, nullptr));
    });
}

TEST(SignalPendingLimit, DefaultIgnoreDispositionReleasesPendingEntries) {
    Isolated([] {
        Block(SIGWINCH);
        Block(SIGRTMIN);
        Limit(1);
        struct sigaction action = {};
        action.sa_handler = IgnoreHandler;
        sigemptyset(&action.sa_mask);
        ASSERT_EQ(0, sigaction(SIGWINCH, &action, nullptr));
        ASSERT_EQ(0, Queue(getpid(), SIGWINCH));
        CheckSigQ(1, 1);
        action.sa_handler = SIG_DFL;
        ASSERT_EQ(0, sigaction(SIGWINCH, &action, nullptr));
        CheckSigQ(0, 1);
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN));
    });
}

TEST(SignalPendingLimit, LimitAccessSurvivesTimerDeliveryAndForkSnapshots) {
    Isolated([] {
        struct sigaction action = {};
        action.sa_handler = CountAlarm;
        sigemptyset(&action.sa_mask);
        ASSERT_EQ(0, sigaction(SIGALRM, &action, nullptr));
        timer_deliveries = 0;
        itimerval timer = {};
        timer.it_value.tv_usec = 1000;
        timer.it_interval.tv_usec = 1000;
        ASSERT_EQ(0, setitimer(ITIMER_REAL, &timer, nullptr));
        bool valid = true;
        for (int i = 0; i < 20000; ++i) {
            const rlimit setting = {static_cast<rlim_t>(63 + (i & 1)), 64};
            rlimit observed = {};
            if (setrlimit(RLIMIT_SIGPENDING, &setting) != 0 ||
                getrlimit(RLIMIT_SIGPENDING, &observed) != 0 ||
                observed.rlim_cur != setting.rlim_cur || observed.rlim_max != 64) {
                valid = false;
                break;
            }
            if (i % 2000 == 0) {
                const pid_t child = fork();
                if (child == 0) {
                    rlimit snapshot = {};
                    const bool matches = getrlimit(RLIMIT_SIGPENDING, &snapshot) == 0 &&
                                         snapshot.rlim_cur == setting.rlim_cur &&
                                         snapshot.rlim_max == 64;
                    _exit(matches ? 0 : 1);
                }
                if (child < 0) {
                    valid = false;
                    break;
                }
                int status = 0;
                pid_t waited;
                do {
                    waited = waitpid(child, &status, 0);
                } while (waited < 0 && errno == EINTR);
                if (waited != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
                    valid = false;
                    break;
                }
            }
        }
        // Very fast hosts may finish before the first deadline. Allow at most
        // 100 ms for an actual delivery, never an unbounded stress loop.
        for (int i = 0; timer_deliveries == 0 && i < 100; ++i) {
            usleep(1000);
        }
        const itimerval cancel = {};
        EXPECT_EQ(0, setitimer(ITIMER_REAL, &cancel, nullptr));
        EXPECT_TRUE(valid);
        EXPECT_GT(timer_deliveries, 0);
    });
}

TEST(SignalPendingLimit, ProcessExitReleasesPendingEntries) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(2);
        Receiver receiver;
        ASSERT_TRUE(receiver.Start(2));
        ASSERT_EQ(0, Queue(receiver.pid(), SIGRTMIN));
        ASSERT_EQ(0, Queue(receiver.pid(), SIGRTMIN));
        ASSERT_TRUE(receiver.Exit());
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN));
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN));
    });
}

TEST(SignalPendingLimit, ZombieRetainsPendingUntilReapedEvenWithPidfd) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(2);
        struct sigaction action = {};
        action.sa_handler = SIG_DFL;
        sigemptyset(&action.sa_mask);
        ASSERT_EQ(0, sigaction(SIGCHLD, &action, nullptr));
        Receiver receiver;
        ASSERT_TRUE(receiver.Start(2));
        const int pidfd = syscall(SYS_pidfd_open, receiver.pid(), 0);
        ASSERT_GE(pidfd, 0);
        ASSERT_EQ(0, Queue(receiver.pid(), SIGRTMIN));
        ASSERT_EQ(0, Queue(receiver.pid(), SIGRTMIN));
        ASSERT_TRUE(receiver.BecomeZombie());
        // A zombie still owns its regular pending records. The default-ignored
        // (unblocked) SIGCHLD does not add a record to this UID's inventory.
        CheckSigQ(2, 2);
        errno = 0;
        EXPECT_EQ(-1, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
        ASSERT_TRUE(receiver.Reap());
        // pidfd pins PID identity, not the dead task's pending resources.
        CheckSigQ(0, 2);
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(0, close(pidfd));
    });
}

TEST(SignalPendingLimit, OrdinaryThreadExitPreservesSharedPending) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(1);
        std::atomic<bool> ready{false};
        std::atomic<bool> exit{false};
        struct Context {
            std::atomic<bool>* ready;
            std::atomic<bool>* exit;
        } context{&ready, &exit};
        pthread_t thread;
        ASSERT_EQ(0, pthread_create(&thread, nullptr, [](void* arg) -> void* {
            auto* context = static_cast<Context*>(arg);
            context->ready->store(true);
            while (!context->exit->load()) {
                sched_yield();
            }
            return nullptr;
        }, &context));
        while (!ready.load()) {
            sched_yield();
        }
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN, 99));
        exit.store(true);
        EXPECT_EQ(0, pthread_join(thread, nullptr));
        errno = 0;
        EXPECT_EQ(-1, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
        siginfo_t info = {};
        ASSERT_EQ(SIGRTMIN, Receive(SIGRTMIN, &info));
        EXPECT_EQ(99, info.si_value.sival_int);
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN));
    });
}

TEST(SignalPendingLimit, TimerReservationDeletedWithoutExpiry) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(2);
        sigevent event = {};
        event.sigev_notify = SIGEV_NONE;
        int first = -1;
        int second = -1;
        int excess = -1;
        ASSERT_EQ(0, syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &first));
        ASSERT_EQ(0, syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &second));
        errno = 0;
        EXPECT_EQ(-1, syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &excess));
        EXPECT_EQ(EAGAIN, errno);
        errno = 0;
        EXPECT_EQ(-1, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
        ASSERT_EQ(0, syscall(SYS_timer_delete, first));
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN));
        siginfo_t info = {};
        ASSERT_EQ(SIGRTMIN, Receive(SIGRTMIN, &info));
        ASSERT_EQ(0, syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &excess));
        EXPECT_EQ(0, syscall(SYS_timer_delete, excess));
        EXPECT_EQ(0, syscall(SYS_timer_delete, second));
    });
}

TEST(SignalPendingLimit, TimerReservationReleasedOnProcessExit) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(1);
        Receiver receiver;
        ASSERT_TRUE(receiver.Start(1, true));
        errno = 0;
        EXPECT_EQ(-1, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
        ASSERT_TRUE(receiver.Exit());
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN));
    });
}

TEST(SignalPendingLimit, TimerReservationReleasedBeforeZombieIsReaped) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(1);
        struct sigaction action = {};
        action.sa_handler = SIG_DFL;
        sigemptyset(&action.sa_mask);
        ASSERT_EQ(0, sigaction(SIGCHLD, &action, nullptr));
        Receiver receiver;
        ASSERT_TRUE(receiver.Start(1, true));
        CheckSigQ(1, 1);
        ASSERT_TRUE(receiver.BecomeZombie());
        // Timer lifetime ends at the last live thread's exit, unlike ordinary
        // pending signals retained by a zombie until its signal state is reaped.
        CheckSigQ(0, 1);
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN));
        ASSERT_TRUE(receiver.Reap());
        CheckSigQ(1, 1);
    });
}

TEST(SignalPendingLimit, TimerExpiryReusesReservationUntilDeletion) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(1);
        sigevent event = {};
        event.sigev_notify = SIGEV_SIGNAL;
        event.sigev_signo = SIGRTMIN;
        event.sigev_value.sival_int = 78;
        int id = -1;
        ASSERT_EQ(0, syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &id));
        itimerspec setting = {};
        setting.it_value.tv_nsec = 1000000;
        ASSERT_EQ(0, syscall(SYS_timer_settime, id, 0, &setting, nullptr));
        sigset_t mask;
        sigemptyset(&mask);
        sigaddset(&mask, SIGRTMIN);
        siginfo_t info = {};
        const timespec timeout = {2, 0};
        ASSERT_EQ(SIGRTMIN, sigtimedwait(&mask, &info, &timeout));
        EXPECT_EQ(SI_TIMER, info.si_code);
        EXPECT_EQ(78, info.si_value.sival_int);
        // Dequeue frees a regular entry, but the timer retains its reservation.
        errno = 0;
        EXPECT_EQ(-1, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
        ASSERT_EQ(0, syscall(SYS_timer_delete, id));
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN));
    });
}

TEST(SignalPendingLimit, DeletingPendingTimerKeepsChargeUntilDequeue) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(1);
        sigevent event = {};
        event.sigev_notify = SIGEV_SIGNAL;
        event.sigev_signo = SIGRTMIN;
        int id = -1;
        ASSERT_EQ(0, syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &id));
        itimerspec setting = {};
        setting.it_value.tv_nsec = 1000000;
        ASSERT_EQ(0, syscall(SYS_timer_settime, id, 0, &setting, nullptr));
        bool pending = false;
        for (int attempt = 0; attempt < 2000; ++attempt) {
            sigset_t mask;
            ASSERT_EQ(0, sigpending(&mask));
            if (sigismember(&mask, SIGRTMIN) == 1) {
                pending = true;
                break;
            }
            usleep(1000);
        }
        ASSERT_TRUE(pending);
        ASSERT_EQ(0, syscall(SYS_timer_delete, id));
        errno = 0;
        EXPECT_EQ(-1, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
        siginfo_t info = {};
        ASSERT_EQ(SIGRTMIN, Receive(SIGRTMIN, &info));
        EXPECT_EQ(SI_TIMER, info.si_code);
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN));
    });
}

TEST(SignalPendingLimit, EffectiveUidDoesNotChangeRealUidAccount) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(1);
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN));
        const uid_t other = getuid() + 10000000U;
        ASSERT_EQ(0, seteuid(0));
        ASSERT_EQ(0, seteuid(other));
        errno = 0;
        EXPECT_EQ(-1, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
        siginfo_t info = {};
        ASSERT_EQ(SIGRTMIN, Receive(SIGRTMIN, &info));
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN));
    }, true);
}

TEST(SignalPendingLimit, RealUidChangePreservesOldPendingOwner) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(1);
        const uid_t first = getuid();
        const uid_t second = first + 10000000U;
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN, 1));
        ASSERT_EQ(0, seteuid(0));
        ASSERT_EQ(0, setresuid(second, second, 0));
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN, 2));
        errno = 0;
        EXPECT_EQ(-1, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
        siginfo_t info = {};
        ASSERT_EQ(SIGRTMIN, Receive(SIGRTMIN, &info));
        EXPECT_EQ(1, info.si_value.sival_int);
        ASSERT_EQ(0, seteuid(0));
        ASSERT_EQ(0, setresuid(first, first, 0));
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN, 3));
        ASSERT_EQ(SIGRTMIN, Receive(SIGRTMIN, &info));
        EXPECT_EQ(2, info.si_value.sival_int);
        ASSERT_EQ(0, seteuid(0));
        ASSERT_EQ(0, setresuid(second, second, 0));
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN, 4));
    }, true);
}

TEST(SignalPendingLimit, ProcStatusCountsPendingAndTimerReservations) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(4);
        CheckSigQ(0, 4);
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN));
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN));
        CheckSigQ(2, 4);
        sigevent event = {};
        event.sigev_notify = SIGEV_NONE;
        int id = -1;
        ASSERT_EQ(0, syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &id));
        CheckSigQ(3, 4);
        siginfo_t info = {};
        ASSERT_EQ(SIGRTMIN, Receive(SIGRTMIN, &info));
        ASSERT_EQ(SIGRTMIN, Receive(SIGRTMIN, &info));
        CheckSigQ(1, 4);
        ASSERT_EQ(0, syscall(SYS_timer_delete, id));
        CheckSigQ(0, 4);
    });
}

TEST(SignalPendingLimit, ConcurrentSendersCannotOverbookSingleSlot) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(1);
        int start[2];
        ASSERT_EQ(0, pipe(start));
        const pid_t target = getpid();
        pid_t senders[2] = {-1, -1};
        for (int i = 0; i < 2; ++i) {
            senders[i] = fork();
            if (senders[i] == 0) {
                alarm(5);
                close(start[1]);
                char token;
                if (read(start[0], &token, 1) != 1) {
                    _exit(2);
                }
                errno = 0;
                const int queued = Queue(target, SIGRTMIN);
                _exit(queued == 0 ? 0 : (errno == EAGAIN ? 1 : 2));
            }
            EXPECT_GT(senders[i], 0);
        }
        close(start[0]);
        EXPECT_EQ(2, write(start[1], "SS", 2));
        close(start[1]);
        int successes = 0;
        int rejected = 0;
        for (pid_t sender : senders) {
            if (sender <= 0) {
                continue;
            }
            int status = 0;
            EXPECT_EQ(sender, waitpid(sender, &status, 0));
            EXPECT_TRUE(WIFEXITED(status));
            if (WIFEXITED(status)) {
                successes += WEXITSTATUS(status) == 0;
                rejected += WEXITSTATUS(status) == 1;
            }
        }
        EXPECT_EQ(1, successes);
        EXPECT_EQ(1, rejected);
        siginfo_t info = {};
        EXPECT_EQ(SIGRTMIN, Receive(SIGRTMIN, &info));
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN));
    });
}

TEST(SignalPendingLimit, SignalfdReadRefundsOneQueuedRecord) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(2);
        sigset_t mask;
        sigemptyset(&mask);
        sigaddset(&mask, SIGRTMIN);
        const int fd = signalfd(-1, &mask, SFD_NONBLOCK | SFD_CLOEXEC);
        ASSERT_GE(fd, 0);
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN, 11));
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN, 22));
        signalfd_siginfo record = {};
        ASSERT_EQ(static_cast<ssize_t>(sizeof(record)), read(fd, &record, sizeof(record)));
        EXPECT_EQ(11, record.ssi_int);
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN, 33));
        errno = 0;
        EXPECT_EQ(-1, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
        EXPECT_EQ(0, close(fd));
        siginfo_t info = {};
        ASSERT_EQ(SIGRTMIN, Receive(SIGRTMIN, &info));
        EXPECT_EQ(22, info.si_value.sival_int);
        ASSERT_EQ(SIGRTMIN, Receive(SIGRTMIN, &info));
        EXPECT_EQ(33, info.si_value.sival_int);
    });
}

TEST(SignalPendingLimit, UserNamespaceAncestorCeilingSurvivesRaisedLocalLimit) {
    Isolated([] {
        Block(SIGRTMIN);
        // The namespace snapshots the creator's soft limit, not the hard one.
        const rlimit creation_limit = {2, 64};
        ASSERT_EQ(0, setrlimit(RLIMIT_SIGPENDING, &creation_limit));
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN, 1));
        if (unshare(CLONE_NEWUSER) != 0) {
            if (errno == EPERM || errno == EACCES) {
                _exit(77);
            }
            FAIL() << "unshare user namespace: " << errno;
        }
        const rlimit raised_limit = {4, 64};
        ASSERT_EQ(0, setrlimit(RLIMIT_SIGPENDING, &raised_limit));
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN, 2));
        errno = 0;
        EXPECT_EQ(-1, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
        siginfo_t info = {};
        ASSERT_EQ(SIGRTMIN, Receive(SIGRTMIN, &info));
        EXPECT_EQ(1, info.si_value.sival_int);
        EXPECT_EQ(0, Queue(getpid(), SIGRTMIN, 3));
    });
}

TEST(SignalPendingLimit, UserNamespaceChargesCreatorEffectiveUid) {
    Isolated([] {
        Block(SIGRTMIN);
        const rlimit creation_limit = {2, 64};
        ASSERT_EQ(0, setrlimit(RLIMIT_SIGPENDING, &creation_limit));
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN, 1));
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN, 2));
        const uid_t creator = getuid() + 10000000U;
        ASSERT_EQ(0, seteuid(0));
        ASSERT_EQ(0, seteuid(creator));
        if (unshare(CLONE_NEWUSER) != 0) {
            if (errno == EPERM || errno == EACCES) {
                _exit(77);
            }
            FAIL() << "unshare user namespace: " << errno;
        }
        // Existing entries belong to parent real UID A. New namespace charges
        // propagate to creator effective UID B, whose two slots are still free.
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN, 3));
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN, 4));
        errno = 0;
        EXPECT_EQ(-1, Queue(getpid(), SIGRTMIN));
        EXPECT_EQ(EAGAIN, errno);
    }, true);
}

TEST(SignalPendingLimit, ExecDropsTimerReservationButKeepsOrdinaryPending) {
    Isolated([] {
        Block(SIGRTMIN);
        Limit(2);
        sigevent event = {};
        event.sigev_notify = SIGEV_NONE;
        int id = -1;
        ASSERT_EQ(0, syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &id));
        ASSERT_EQ(0, Queue(getpid(), SIGRTMIN, 123));
        // The host checkout may live below root-only directories. Follow the
        // executable magic link without reopening that inaccessible pathname.
        execl("/proc/self/exe", "signal_pending_limit_test",
              "--signal-quota-exec-helper", nullptr);
        FAIL() << "exec: " << errno;
    });
}

int main(int argc, char** argv) {
    if (argc == 2 && strcmp(argv[1], "--signal-quota-exec-helper") == 0) {
        unsigned long long used = 0;
        unsigned long long limit = 0;
        if (!ReadSigQ(&used, &limit) || used != 1 || limit != 2) {
            return 100;
        }
        if (Queue(getpid(), SIGRTMIN, 456) != 0) {
            return 101;
        }
        errno = 0;
        if (Queue(getpid(), SIGRTMIN) != -1 || errno != EAGAIN) {
            return 102;
        }
        siginfo_t info = {};
        if (Receive(SIGRTMIN, &info) != SIGRTMIN || info.si_value.sival_int != 123) {
            return 103;
        }
        if (Receive(SIGRTMIN, &info) != SIGRTMIN || info.si_value.sival_int != 456) {
            return 104;
        }
        return 0;
    }
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
