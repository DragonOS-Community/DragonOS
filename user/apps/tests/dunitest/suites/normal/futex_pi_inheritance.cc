#include <gtest/gtest.h>

#include <atomic>
#include <cerrno>
#include <csignal>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

namespace {

int64_t Now(clockid_t clock = CLOCK_MONOTONIC) {
    timespec t{};
    if (clock_gettime(clock, &t)) _exit(90);
    return int64_t(t.tv_sec) * 1000000000 + t.tv_nsec;
}

long Pi(std::atomic<uint32_t>& word, int operation) {
    const int64_t deadline = Now(CLOCK_REALTIME) + 4000000000LL;
    const timespec t{deadline / 1000000000, deadline % 1000000000};
    return syscall(SYS_futex, &word, operation | FUTEX_PRIVATE_FLAG, 0,
                   operation == FUTEX_LOCK_PI ? &t : nullptr, nullptr, 0);
}

int Policy(pid_t tid, int policy, int priority) {
    sched_param p{};
    p.sched_priority = priority;
    return sched_setscheduler(tid, policy, &p);
}

template <typename Predicate> bool Until(Predicate ready) {
    const int64_t end = Now() + 2000000000;
    while (!ready() && Now() < end) usleep(1000);
    return ready();
}

bool Sleeping(pid_t tid) {
    char path[64], text[512];
    std::snprintf(path, sizeof(path), "/proc/%d/stat", tid);
    FILE* input = std::fopen(path, "r");
    if (!input) return false;
    const bool read = std::fgets(text, sizeof(text), input) != nullptr;
    std::fclose(input);
    const char* end = read ? std::strrchr(text, ')') : nullptr;
    return end && end[1] == ' ' && end[2] == 'S';
}

int PinCpu(int cpu) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    return sched_setaffinity(0, sizeof(set), &set);
}

int PrepareController() {
    signal(SIGALRM, SIG_DFL);
    alarm(7);
    cpu_set_t allowed;
    if (sched_getaffinity(0, sizeof(allowed), &allowed)) return -1;
    for (int i = 0; i < CPU_SETSIZE; ++i) {
        if (CPU_ISSET(i, &allowed)) {
            if (PinCpu(i)) return -1;
            if (Policy(0, SCHED_FIFO, 90)) return errno == EPERM ? -77 : -1;
            return i;
        }
    }
    return -1;
}

// A FIFO90 controller blocks after making every measured worker runnable.
// Both M and H run on the same CPU as the owner; M is bounded to 200ms so the
// pre-fix kernel completes too. Only H acquiring before M finishes proves PI.
class Scenario {
public:
    enum Change { KeepBase, Renice, ReplaceBase, RaiseDonor, LowerDonor };

    Scenario(int owner_policy, Change change, bool chain)
        : owner_policy_(owner_policy), change_(change), chain_(chain) {}

    int Run() {
        signal(SIGALRM, SIG_DFL);
        alarm(7);
        cpu_set_t allowed;
        if (sched_getaffinity(0, sizeof(allowed), &allowed)) return 10;
        for (int i = 0; i < CPU_SETSIZE; ++i) {
            if (CPU_ISSET(i, &allowed)) { cpu_ = i; break; }
        }
        if (cpu_ < 0 || Pin()) return 11;
        if (Policy(0, SCHED_FIFO, 90)) return errno == EPERM ? 77 : 12;
        for (auto& pipe : gates_) if (::pipe(pipe)) return 13;
        const int count = chain_ ? 4 : 3;
        for (int i = 0; i < count; ++i) {
            args_[i] = {this, i};
            if (pthread_create(&workers_[i], nullptr, Worker, &args_[i])) return 14;
        }
        if (!Until([&] { return ready_.load() == (1u << count) - 1; })) return 15;
        for (int i = 0; i < count; ++i) if (errors_[i]) return 16;
        if (chain_) {
            Start(3);  // The bridge already owns A; now block it on B.
            if (!Until([&] { return (b_.load() & FUTEX_WAITERS) != 0; })) return 17;
        }
        Start(1);
        if (!Until([&] { return (a_.load() & FUTEX_WAITERS) != 0; })) return 18;

        // Mutation while H is blocked must preserve its effective donation.
        int expected_policy = owner_policy_;
        int expected_priority = owner_policy_ == SCHED_OTHER ? 0 : 10;
        if (change_ == Renice) {
            if (setpriority(PRIO_PROCESS, owner_tid_.load(), 5)) return 19;
        } else if (change_ == ReplaceBase) {
            if (Policy(owner_tid_.load(), SCHED_RR, 20)) return 20;
            expected_policy = SCHED_RR;
            expected_priority = 20;
        } else if (change_ == RaiseDonor || change_ == LowerDonor) {
            sched_param param{};
            param.sched_priority = change_ == RaiseDonor ? 80 : 40;
            if (sched_setparam(high_tid_.load(), &param)) return 29;
        }
        sched_param p{};
        if (sched_getscheduler(owner_tid_.load()) != expected_policy ||
            sched_getparam(owner_tid_.load(), &p) || p.sched_priority != expected_priority)
            return 21;
        Start(2);
        Start(0);
        usleep(500000);
        for (int i = 0; i < count; ++i) pthread_join(workers_[i], nullptr);
        Policy(0, SCHED_OTHER, 0);
        alarm(0);
        for (int i = 0; i < count; ++i) if (errors_[i]) return 22;
        std::printf("PI cpu=%d base=%d change=%d chain=%d H=%d M=%d owner_after_unlock=%d\n",
                    cpu_, owner_policy_, change_, chain_, high_order_, medium_order_, owner_order_);
        if (a_.load() || b_.load()) return 23;
        if (!high_order_ || !medium_order_ || !owner_order_) return 24;
        const bool inherited_before_medium = high_order_ < medium_order_;
        if (inherited_before_medium != (change_ != LowerDonor)) return 25;
        if (medium_order_ >= owner_order_) return 26; // Actual deboost.
        if (observed_policy_ != expected_policy || observed_priority_ != expected_priority) return 27;
        if (change_ == Renice && observed_nice_ != 5) return 28;
        return 0;
    }

private:
    struct Argument { Scenario* scenario; int index; };

    int Pin() {
        cpu_set_t set;
        CPU_ZERO(&set);
        CPU_SET(cpu_, &set);
        return sched_setaffinity(0, sizeof(set), &set);
    }
    template <typename Predicate> bool Until(Predicate ready) {
        const int64_t end = Now() + 2000000000;
        while (!ready() && Now() < end) usleep(1000);
        return ready();
    }
    void Start(int i) {
        if (write(gates_[i][1], "x", 1) != 1) _exit(91);
    }
    bool Gate(int i) {
        char c;
        return read(gates_[i][0], &c, 1) == 1;
    }
    void Fail(int i) { errors_[i] = errno ? errno : EIO; }
    int Order() { return sequence_.fetch_add(1) + 1; }
    static void* Worker(void* raw) {
        auto* arg = static_cast<Argument*>(raw);
        arg->scenario->Work(arg->index);
        return nullptr;
    }
    void Work(int i) {
        const int kind = i == 0 ? owner_policy_ : (i == 3 ? SCHED_OTHER : SCHED_FIFO);
        const int priority = i == 1 ? (change_ == RaiseDonor ? 40 : 80)
                                   : (i == 2 ? 50 : (kind == SCHED_OTHER ? 0 : 10));
        if (Pin() || Policy(0, kind, priority)) Fail(i);
        if (i == 1) high_tid_.store(syscall(SYS_gettid));
        if (i == 0) {
            owner_tid_.store(syscall(SYS_gettid));
            if (!errors_[i] && Pi(chain_ ? b_ : a_, FUTEX_LOCK_PI)) Fail(i);
        } else if (i == 3 && !errors_[i] && Pi(a_, FUTEX_LOCK_PI)) {
            Fail(i);
        }
        ready_.fetch_or(1u << i);
        if (errors_[i] || !Gate(i)) return;
        if (i == 0) {
            if (Pi(chain_ ? b_ : a_, FUTEX_UNLOCK_PI)) Fail(i);
            // If donation was revoked, runnable FIFO50 M must run before this
            // low-priority owner can execute the following userspace store.
            owner_order_ = Order();
            observed_policy_ = sched_getscheduler(0);
            sched_param p{};
            if (sched_getparam(0, &p)) Fail(i);
            observed_priority_ = p.sched_priority;
            errno = 0;
            observed_nice_ = getpriority(PRIO_PROCESS, 0);
            if (errno) Fail(i);
        } else if (i == 1) {
            if (Pi(a_, FUTEX_LOCK_PI)) Fail(i);
            else {
                high_order_ = Order();
                if (Pi(a_, FUTEX_UNLOCK_PI)) Fail(i);
            }
        } else if (i == 2) {
            const int64_t end = Now() + 200000000;
            while (Now() < end) std::atomic_signal_fence(std::memory_order_seq_cst);
            medium_order_ = Order();
        } else {
            if (Pi(b_, FUTEX_LOCK_PI)) Fail(i);
            else if (Pi(b_, FUTEX_UNLOCK_PI)) Fail(i);
            if (Pi(a_, FUTEX_UNLOCK_PI)) Fail(i);
        }
    }

    int owner_policy_, cpu_ = -1;
    Change change_;
    bool chain_;
    int gates_[4][2]{}, errors_[4]{};
    pthread_t workers_[4]{};
    Argument args_[4]{};
    std::atomic<uint32_t> a_{0}, b_{0};
    std::atomic<unsigned> ready_{0};
    std::atomic<int> sequence_{0}, owner_tid_{0}, high_tid_{0};
    int high_order_ = 0, medium_order_ = 0, owner_order_ = 0;
    int observed_policy_ = -1, observed_priority_ = -1, observed_nice_ = -1;
};

void RunScenario(int policy, Scenario::Change change, bool chain = false) {
    // Isolate RT policies, affinity, child watchdog and failures from gtest.
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        const int result = Scenario(policy, change, chain).Run();
        std::fflush(stdout);
        _exit(result);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status)) << "worker killed by signal " << WTERMSIG(status);
    if (WEXITSTATUS(status) == 77) GTEST_SKIP() << "SCHED_FIFO requires CAP_SYS_NICE or RLIMIT_RTPRIO";
    EXPECT_EQ(0, WEXITSTATUS(status)) << "PI scenario failure code " << WEXITSTATUS(status);
}

struct QueueWaiter {
    std::atomic<uint32_t>* word;
    std::atomic<int>* sequence;
    std::atomic<pid_t> tid{0};
    std::atomic<bool> finished{false};
    int cpu = -1, policy = SCHED_FIFO, priority = 0, nice = 0;
    int timeout_ms = 4000;
    int order = 0, error = 0;

    static void* Run(void* raw) {
        auto* self = static_cast<QueueWaiter*>(raw);
        if (PinCpu(self->cpu) || Policy(0, self->policy, self->priority) ||
            setpriority(PRIO_PROCESS, 0, self->nice)) {
            self->error = errno;
            return nullptr;
        }
        self->tid.store(syscall(SYS_gettid));
        const int64_t deadline = Now(CLOCK_REALTIME) + int64_t(self->timeout_ms) * 1000000;
        const timespec t{deadline / 1000000000, deadline % 1000000000};
        if (syscall(SYS_futex, self->word, FUTEX_LOCK_PI | FUTEX_PRIVATE_FLAG, 0,
                    &t, nullptr, 0)) self->error = errno;
        else {
            self->order = self->sequence->fetch_add(1) + 1;
            if (Pi(*self->word, FUTEX_UNLOCK_PI)) self->error = errno;
        }
        self->finished.store(true);
        return nullptr;
    }
};

int QueueOrder(bool fair, int reprioritize = 0) {
    const int cpu = PrepareController();
    if (cpu < 0) return cpu == -77 ? 77 : 10;
    std::atomic<uint32_t> word{0};
    std::atomic<int> sequence{0};
    if (Pi(word, FUTEX_LOCK_PI)) return 11;
    QueueWaiter waiters[3];
    pthread_t threads[3];
    const int count = fair || reprioritize ? 2 : 3;
    for (int i = 0; i < count; ++i) {
        waiters[i].word = &word;
        waiters[i].sequence = &sequence;
        waiters[i].cpu = cpu;
        waiters[i].policy = fair ? SCHED_OTHER : SCHED_FIFO;
        waiters[i].priority = fair ? 0 : (i == 0 ? 40 : 80);
        if (reprioritize < 0) waiters[i].priority = i == 0 ? 80 : 40;
        waiters[i].nice = fair && i == 1 ? -10 : 0;
        if (pthread_create(&threads[i], nullptr, QueueWaiter::Run, &waiters[i])) return 12;
        // The worker publishes tid after every setup operation. Its only
        // subsequent blocking point is LOCK_PI, so S proves registration.
        if (!Until([&] { return waiters[i].tid.load() && Sleeping(waiters[i].tid.load()); })) return 13;
    }
    if (reprioritize) {
        sched_param param{};
        param.sched_priority = reprioritize > 0 ? 80 : 40;
        if (sched_setparam(waiters[0].tid.load(), &param)) return 18;
    }
    if (Pi(word, FUTEX_UNLOCK_PI)) return 14;
    usleep(100000);
    for (int i = 0; i < count; ++i) pthread_join(threads[i], nullptr);
    for (int i = 0; i < count; ++i) if (waiters[i].error) return 15;
    // Linux rt_mutex_adjust_prio_chain dequeues a waiter whose sort key
    // changed and inserts it again. At the new equal priority the already
    // queued B precedes A, even though A entered this futex first.
    if (reprioritize) return waiters[1].order == 1 && waiters[0].order == 2 ? 0 : 19;
    if (fair) return waiters[0].order == 1 && waiters[1].order == 2 ? 0 : 16;
    return waiters[1].order == 1 && waiters[2].order == 2 && waiters[0].order == 3 ? 0 : 17;
}

struct DeadlockWaiter {
    std::atomic<uint32_t> a{0}, b{0};
    std::atomic<pid_t> tid{0};
    int cpu = -1, error = 0;
    static void* Run(void* raw) {
        auto* self = static_cast<DeadlockWaiter*>(raw);
        if (PinCpu(self->cpu) || Policy(0, SCHED_OTHER, 0) || Pi(self->b, FUTEX_LOCK_PI)) {
            self->error = errno;
            return nullptr;
        }
        self->tid.store(syscall(SYS_gettid));
        if (Pi(self->a, FUTEX_LOCK_PI)) self->error = errno;
        else if (Pi(self->a, FUTEX_UNLOCK_PI)) self->error = errno;
        if (Pi(self->b, FUTEX_UNLOCK_PI)) self->error = errno;
        return nullptr;
    }
};

int ChainDeadlock(bool try_only = false) {
    const int cpu = PrepareController();
    if (cpu < 0) return cpu == -77 ? 77 : 10;
    DeadlockWaiter state;
    state.cpu = cpu;
    if (Pi(state.a, FUTEX_LOCK_PI)) return 11;
    pthread_t worker;
    if (pthread_create(&worker, nullptr, DeadlockWaiter::Run, &state)) return 12;
    if (!Until([&] { return state.tid.load() && Sleeping(state.tid.load()); })) return 13;
    // Controller owns A and the worker owns B while blocked on A. A plain
    // self-TID check cannot detect this cycle. The old implementation times
    // out; unlock below bounds cleanup on both the broken and fixed kernels.
    errno = 0;
    const long result = Pi(state.b, try_only ? FUTEX_TRYLOCK_PI : FUTEX_LOCK_PI);
    const int error = errno;
    if (result == 0) Pi(state.b, FUTEX_UNLOCK_PI);
    errno = 0;
    const long self_result = try_only ? Pi(state.a, FUTEX_TRYLOCK_PI) : -1;
    const int self_error = errno;
    if (Pi(state.a, FUTEX_UNLOCK_PI)) return 14;
    usleep(100000);
    pthread_join(worker, nullptr);
    if (state.error) return 15;
    const int expected_error = try_only ? EAGAIN : EDEADLK;
    if (try_only && (self_result != -1 || self_error != EDEADLK)) return 17;
    return result == -1 && error == expected_error && state.a.load() == 0 && state.b.load() == 0 ? 0 : 16;
}

struct MultipleDonors {
    std::atomic<uint32_t> a{0}, b{0};
    std::atomic<int> sequence{0};
    std::atomic<pid_t> owner_tid{0};
    std::atomic<bool> medium_ready{false};
    int owner_gate[2]{}, medium_gate[2]{};
    int cpu = -1, medium_priority = 50, owner_error = 0, medium_error = 0;
    int medium_order = 0;

    static void* Owner(void* raw) {
        auto* self = static_cast<MultipleDonors*>(raw);
        if (PinCpu(self->cpu) || Policy(0, SCHED_OTHER, 0) ||
            Pi(self->a, FUTEX_LOCK_PI) || Pi(self->b, FUTEX_LOCK_PI)) {
            self->owner_error = errno;
            return nullptr;
        }
        self->owner_tid.store(syscall(SYS_gettid));
        char c;
        if (read(self->owner_gate[0], &c, 1) != 1 ||
            Pi(self->a, FUTEX_UNLOCK_PI) || Pi(self->b, FUTEX_UNLOCK_PI))
            self->owner_error = errno ? errno : EIO;
        return nullptr;
    }
    static void* Medium(void* raw) {
        auto* self = static_cast<MultipleDonors*>(raw);
        if (PinCpu(self->cpu) || Policy(0, SCHED_FIFO, self->medium_priority)) {
            self->medium_error = errno;
            self->medium_ready.store(true);
            return nullptr;
        }
        self->medium_ready.store(true);
        char c;
        if (read(self->medium_gate[0], &c, 1) != 1) {
            self->medium_error = errno ? errno : EIO;
            return nullptr;
        }
        const int64_t end = Now() + 200000000;
        while (Now() < end) std::atomic_signal_fence(std::memory_order_seq_cst);
        self->medium_order = self->sequence.fetch_add(1) + 1;
        return nullptr;
    }
};

int CancelTopDonor(int medium_priority) {
    const int cpu = PrepareController();
    if (cpu < 0) return cpu == -77 ? 77 : 10;
    MultipleDonors state;
    state.cpu = cpu;
    state.medium_priority = medium_priority;
    if (pipe(state.owner_gate) || pipe(state.medium_gate)) return 11;
    pthread_t owner, medium;
    if (pthread_create(&owner, nullptr, MultipleDonors::Owner, &state) ||
        pthread_create(&medium, nullptr, MultipleDonors::Medium, &state)) return 12;
    if (!Until([&] {
            return state.owner_tid.load() && Sleeping(state.owner_tid.load()) && state.medium_ready.load();
        })) return 13;
    QueueWaiter waiters[2];
    pthread_t threads[2];
    for (int i = 0; i < 2; ++i) {
        waiters[i].word = i == 0 ? &state.a : &state.b;
        waiters[i].sequence = &state.sequence;
        waiters[i].cpu = cpu;
        waiters[i].priority = i == 0 ? 80 : 60;
        waiters[i].timeout_ms = i == 0 ? 300 : 4000;
        if (pthread_create(&threads[i], nullptr, QueueWaiter::Run, &waiters[i])) return 14;
        if (!Until([&] { return waiters[i].tid.load() && Sleeping(waiters[i].tid.load()); })) return 15;
    }
    if (!Until([&] { return waiters[0].finished.load(); })) return 16;
    pthread_join(threads[0], nullptr);
    if (waiters[0].error != ETIMEDOUT || waiters[0].order) return 17;
    if (write(state.medium_gate[1], "x", 1) != 1 || write(state.owner_gate[1], "x", 1) != 1) return 18;
    usleep(500000);
    pthread_join(owner, nullptr);
    pthread_join(medium, nullptr);
    pthread_join(threads[1], nullptr);
    if (state.owner_error || state.medium_error || waiters[1].error) return 19;
    if (!state.medium_order || !waiters[1].order || state.a.load() || state.b.load()) return 20;
    // Donor80 timed out: donor60 must remain. M50 must lose to the owner,
    // while M70 must win. These two cases detect both over- and under-revoke.
    const bool donor_ran_first = waiters[1].order < state.medium_order;
    return donor_ran_first == (medium_priority < 60) ? 0 : 21;
}

struct MigrationState {
    std::atomic<uint32_t> word{0};
    std::atomic<pid_t> owner_tid{0};
    std::atomic<bool> release{false}, mover_done{false};
    std::atomic<unsigned> owner_steps{0};
    std::atomic<int> owner_cpu{-1};
    int controller_cpu = -1, remote_cpu = -1;
    int owner_error = 0, mover_error = 0, owner_base = -1;

    static void* Owner(void* raw) {
        auto* self = static_cast<MigrationState*>(raw);
        if (PinCpu(self->remote_cpu) || Policy(0, SCHED_OTHER, 0) ||
            Pi(self->word, FUTEX_LOCK_PI)) {
            self->owner_error = errno;
            return nullptr;
        }
        self->owner_tid.store(syscall(SYS_gettid));
        const int64_t end = Now() + 4000000000LL;
        while (!self->release.load() && Now() < end) {
            unsigned cpu = 0;
            if (syscall(SYS_getcpu, &cpu, nullptr, nullptr)) {
                self->owner_error = errno;
                break;
            }
            self->owner_cpu.store(cpu);
            self->owner_steps.fetch_add(1);
        }
        if (!self->release.load() && !self->owner_error) self->owner_error = ETIMEDOUT;
        if (Pi(self->word, FUTEX_UNLOCK_PI)) self->owner_error = errno;
        self->owner_base = sched_getscheduler(0);
        return nullptr;
    }

    static void* Mover(void* raw) {
        auto* self = static_cast<MigrationState*>(raw);
        if (PinCpu(self->controller_cpu) || Policy(0, SCHED_FIFO, 85)) {
            self->mover_error = errno;
            self->mover_done.store(true);
            return nullptr;
        }
        for (int i = 0; i < 24; ++i) {
            const int target = i % 2 == 0 ? self->controller_cpu : self->remote_cpu;
            const unsigned previous = self->owner_steps.load();
            cpu_set_t set;
            CPU_ZERO(&set);
            CPU_SET(target, &set);
            if (sched_setaffinity(self->owner_tid.load(), sizeof(set), &set)) {
                self->mover_error = errno;
                break;
            }
            // Verify actual post-migration execution, rather than only an
            // affinity mask or scheduler metadata update. Sleeping here lets
            // the boosted owner run on the controller CPU too.
            if (!Until([&] {
                    return self->owner_steps.load() > previous && self->owner_cpu.load() == target;
                })) {
                self->mover_error = ETIMEDOUT;
                break;
            }
        }
        self->mover_done.store(true);
        return nullptr;
    }
};

int MigrateWithBlockedDonorUpdates() {
    cpu_set_t allowed;
    if (sched_getaffinity(0, sizeof(allowed), &allowed)) return 10;
    int second_cpu = -1, seen = 0;
    for (int i = 0; i < CPU_SETSIZE; ++i) {
        if (CPU_ISSET(i, &allowed) && ++seen == 2) { second_cpu = i; break; }
    }
    if (second_cpu < 0) return 78;
    const int cpu = PrepareController();
    if (cpu < 0) return cpu == -77 ? 77 : 11;
    MigrationState state;
    state.controller_cpu = cpu;
    state.remote_cpu = second_cpu;
    pthread_t owner, donor_thread, mover;
    if (pthread_create(&owner, nullptr, MigrationState::Owner, &state)) return 12;
    if (!Until([&] { return state.owner_tid.load() && state.owner_steps.load(); })) return 13;
    std::atomic<int> sequence{0};
    QueueWaiter donor;
    donor.word = &state.word;
    donor.sequence = &sequence;
    donor.cpu = cpu;
    donor.priority = 80;
    if (pthread_create(&donor_thread, nullptr, QueueWaiter::Run, &donor)) return 14;
    if (!Until([&] { return donor.tid.load() && Sleeping(donor.tid.load()); })) return 15;
    if (pthread_create(&mover, nullptr, MigrationState::Mover, &state)) return 16;
    int error = 0;
    for (int i = 0; i < 96 && !state.mover_done.load(); ++i) {
        sched_param param{};
        param.sched_priority = i % 2 == 0 ? 40 : 80;
        if (sched_setparam(donor.tid.load(), &param) ||
            setpriority(PRIO_PROCESS, state.owner_tid.load(), i % 2 == 0 ? 5 : 0)) {
            error = errno;
            break;
        }
        // Base queries must remain Normal while a blocked remote donor
        // changes the effective class and affinity migrates the runnable owner.
        if (sched_getscheduler(state.owner_tid.load()) != SCHED_OTHER) { error = EINVAL; break; }
        usleep(1000);
    }
    pthread_join(mover, nullptr);
    state.release.store(true);
    usleep(100000);
    pthread_join(owner, nullptr);
    pthread_join(donor_thread, nullptr);
    if (error || state.owner_error || state.mover_error || donor.error) return 17;
    if (donor.order != 1 || state.word.load() || state.owner_base != SCHED_OTHER) return 18;
    return 0;
}

void RunIsolated(int (*scenario)()) {
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        const int result = scenario();
        std::fflush(stdout);
        _exit(result);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    if (WEXITSTATUS(status) == 77) GTEST_SKIP() << "SCHED_FIFO requires CAP_SYS_NICE or RLIMIT_RTPRIO";
    if (WEXITSTATUS(status) == 78) GTEST_SKIP() << "migration coverage requires two allowed CPUs";
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(PiInheritance, NormalOwnerRunsBeforeMediumAndDeboosts) {
    RunScenario(SCHED_OTHER, Scenario::KeepBase);
}
TEST(PiInheritance, FifoOwnerRunsBeforeMediumAndDeboosts) {
    RunScenario(SCHED_FIFO, Scenario::KeepBase);
}
TEST(PiInheritance, RrOwnerRunsBeforeMediumAndDeboosts) {
    RunScenario(SCHED_RR, Scenario::KeepBase);
}
TEST(PiInheritance, RenicePreservesRealtimeDonation) {
    RunScenario(SCHED_OTHER, Scenario::Renice);
}
TEST(PiInheritance, ReplacingBasePolicyPreservesDonation) {
    RunScenario(SCHED_OTHER, Scenario::ReplaceBase);
}
TEST(PiInheritance, DonationPropagatesThroughBlockedOwner) {
    RunScenario(SCHED_OTHER, Scenario::KeepBase, true);
}
TEST(PiInheritance, RaisingBlockedDonorPriorityRefreshesOwner) {
    RunScenario(SCHED_OTHER, Scenario::RaiseDonor);
}
TEST(PiInheritance, RaisingBlockedDonorPriorityPropagatesThroughChain) {
    RunScenario(SCHED_OTHER, Scenario::RaiseDonor, true);
}
TEST(PiInheritance, LoweringBlockedDonorPriorityRevokesExcessBoost) {
    RunScenario(SCHED_OTHER, Scenario::LowerDonor);
}
TEST(PiInheritance, RealtimeWaitersUsePriorityThenFifo) {
    RunIsolated(+[] { return QueueOrder(false); });
}
TEST(PiInheritance, FairWaitersKeepFifoAcrossNiceValues) {
    RunIsolated(+[] { return QueueOrder(true); });
}
TEST(PiInheritance, DetectsTwoOwnerCycleAndRollsBackRegistration) {
    RunIsolated(+[] { return ChainDeadlock(); });
}
TEST(PiInheritance, RaisedWaiterRejoinsBehindExistingEqualPriority) {
    RunIsolated(+[] { return QueueOrder(false, 1); });
}
TEST(PiInheritance, LoweredWaiterRejoinsBehindExistingEqualPriority) {
    RunIsolated(+[] { return QueueOrder(false, -1); });
}
TEST(PiInheritance, TrylockPotentialCycleIsBusyAndSelfOwnerIsDeadlock) {
    RunIsolated(+[] { return ChainDeadlock(true); });
}
TEST(PiInheritance, TopTimeoutRetainsOtherOwnedLockDonation) {
    RunIsolated(+[] { return CancelTopDonor(50); });
}
TEST(PiInheritance, TopTimeoutRevokesOnlyTheExpiredPriority) {
    RunIsolated(+[] { return CancelTopDonor(70); });
}
TEST(PiInheritance, AffinityMigrationWithBlockedDonorAndBaseUpdates) {
    RunIsolated(MigrateWithBlockedDonorUpdates);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
