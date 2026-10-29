// Regression coverage for a blocked AF_UNIX `accept()` racing with closure of
// the final listener descriptor, and for `shutdown(listener, SHUT_RD)`.
//
// Ground rules for this file (do not regress them):
//
//  * The accept side MUST run in a *thread*, not a forked process: threads share
//    the descriptor table, so `close(listener)` from another thread really does
//    drop the last reference. `fork`/`dup`/`SCM_RIGHTS` only clone the
//    `Arc<File>`, which would keep the socket alive and hide the bug.
//  * A connector child MUST be forked *after* the listener has been closed in
//    the parent, otherwise it inherits a copy of the listening descriptor and
//    again masks the regression.
//  * "Is it blocked?" is decided by reading `/proc/<pid>/task/<tid>/stat`
//    (`S`/`D`), never by a `poll(timeout) == 0` guess.
//  * Thread-directed signals use `pthread_kill`; `kill(getpid(), ...)` may be
//    delivered to an unrelated thread and makes the test flaky.
//  * Every wait is bounded, and the acceptor threads / connector children are
//    owned by `AcceptorGuard` / `ConnectorHandle`, so they are stopped and
//    reaped even when a failing `ASSERT_*` returns from a case early.

#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sys/epoll.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

#include <atomic>

namespace {

constexpr int kBacklogWaitMs = 5000;
constexpr int kResultWaitMs = 5000;
// Grace period for a second connector to report after one backlog slot was
// freed. Keeping it short bounds the passing path; the ownership check that
// follows does not depend on it.
constexpr int kSecondConnectorGraceMs = 250;
constexpr char kPayload = 'x';
// Upper bound on the connectors `WaitAnyConnectorOutcome()` may be given. It is
// a named constant so that a case which watches more of them fails with a clear
// message instead of writing past the end of the helper's fixed-size arrays.
constexpr size_t kMaxWaitingConnectors = 2;

class ScopedFd {
public:
    explicit ScopedFd(int fd = -1) : fd_(fd) {}
    ~ScopedFd() {
        if (fd_ >= 0) close(fd_);
    }
    ScopedFd(const ScopedFd&) = delete;
    ScopedFd& operator=(const ScopedFd&) = delete;

    int get() const { return fd_; }
    void reset(int fd = -1) {
        if (fd_ >= 0) close(fd_);
        fd_ = fd;
    }
    int release() {
        const int fd = fd_;
        fd_ = -1;
        return fd;
    }

private:
    int fd_;
};

struct AbstractAddress {
    sockaddr_un addr{};
    socklen_t len = 0;
};

// Abstract namespace address: nothing to unlink, so a failing assertion cannot
// leave a stale socket file behind for the next case.
AbstractAddress MakeAbstractAddress(const char* tag) {
    AbstractAddress out;
    out.addr.sun_family = AF_UNIX;
    const int name_len = snprintf(out.addr.sun_path + 1, sizeof(out.addr.sun_path) - 1,
                                  "accept-close-%d-%s", getpid(), tag);
    EXPECT_GT(name_len, 0);
    out.len = static_cast<socklen_t>(offsetof(sockaddr_un, sun_path) + 1 + name_len);
    return out;
}

int MakeListener(const AbstractAddress& address, int backlog) {
    const int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    if (listener < 0) return -1;
    if (bind(listener, reinterpret_cast<const sockaddr*>(&address.addr), address.len) != 0 ||
        listen(listener, backlog) != 0) {
        close(listener);
        return -1;
    }
    return listener;
}

// Read the process/thread state character from `/proc/<tgid>/task/<tid>/stat`
// (or `/proc/<pid>/stat`). Returns '\0' when it cannot be read.
char ReadState(pid_t tgid, pid_t tid, bool per_thread) {
    char path[64] = {};
    if (per_thread) {
        snprintf(path, sizeof(path), "/proc/%d/task/%d/stat", tgid, tid);
    } else {
        snprintf(path, sizeof(path), "/proc/%d/stat", tgid);
    }

    FILE* stat = fopen(path, "r");
    if (stat == nullptr) return '\0';
    char line[512] = {};
    const bool read = fgets(line, sizeof(line), stat) != nullptr;
    fclose(stat);
    if (!read) return '\0';

    // `comm` is parenthesised and may itself contain spaces or parentheses.
    const char* comm_end = strrchr(line, ')');
    if (comm_end == nullptr || comm_end[1] != ' ') return '\0';
    return comm_end[2];
}

// Bounded wait until the task is provably off-CPU in the socket wait.
bool WaitThreadSleeping(pid_t tid, int timeout_ms = kBacklogWaitMs) {
    for (int waited = 0; waited <= timeout_ms; waited += 1) {
        const char state = ReadState(getpid(), tid, true);
        if (state == 'S' || state == 'D') return true;
        usleep(1000);
    }
    return false;
}

bool WaitProcessSleeping(pid_t pid, int timeout_ms = kBacklogWaitMs) {
    for (int waited = 0; waited <= timeout_ms; waited += 1) {
        const char state = ReadState(pid, 0, false);
        if (state == 'S' || state == 'D') return true;
        usleep(1000);
    }
    return false;
}

// ---------------------------------------------------------------------------
// Acceptor thread
// ---------------------------------------------------------------------------

struct AcceptorArgs {
    // Hard bound on `fds`; `StartAcceptor()` rejects a larger `want`, so a case
    // cannot make the worker write past the end of the array.
    static constexpr int kMaxAccepted = 4;

    int listener = -1;
    int want = 1;
    // The thread id is published so the parent can check `/proc` state.
    std::atomic<pid_t> tid{-1};
    std::atomic<int> count{0};
    std::atomic<int> last_errno{0};
    std::atomic<int> fds[kMaxAccepted];
    // Set by the worker as its last action. `AcceptorGuard` uses it instead of
    // `/proc` state to decide whether the thread still needs cancelling, which
    // keeps the passing path free of signals.
    std::atomic<bool> finished{false};
};

void* AcceptWorker(void* raw) {
    AcceptorArgs* args = static_cast<AcceptorArgs*>(raw);
    args->tid.store(static_cast<pid_t>(syscall(SYS_gettid)));
    for (int i = 0; i < args->want; ++i) {
        const int fd = accept(args->listener, nullptr, nullptr);
        if (fd < 0) {
            args->last_errno.store(errno);
            break;
        }
        args->fds[i].store(fd);
        args->count.fetch_add(1);
    }
    args->finished.store(true);
    return nullptr;
}

int StartAcceptor(AcceptorArgs* args, pthread_t* thread) {
    if (args->want < 0 || args->want > AcceptorArgs::kMaxAccepted) return EINVAL;
    for (auto& fd : args->fds) fd.store(-1);
    return pthread_create(thread, nullptr, AcceptWorker, args);
}

pid_t AcceptorTid(const AcceptorArgs& args) {
    for (int i = 0; i < kBacklogWaitMs && args.tid.load() < 0; ++i) usleep(1000);
    return args.tid.load();
}

// Wait (bounded) for an acceptor worker to leave its accept loop. The worker
// publishes this itself, so a slow `/proc` read can never be mistaken for an
// exit.
bool WaitAcceptorFinished(const AcceptorArgs& args, int timeout_ms = kResultWaitMs) {
    for (int waited = 0; waited <= timeout_ms && !args.finished.load(); waited += 1) {
        usleep(1000);
    }
    return args.finished.load();
}

// Assert that an acceptor returned from `accept()` on its own. A stuck
// `accept()` is reported here instead of hanging the suite; joining the thread is
// the `AcceptorGuard`'s job, so a failing assertion still cleans up.
void ExpectAcceptorExited(const AcceptorArgs& args, const char* what) {
    ASSERT_TRUE(WaitAcceptorFinished(args)) << what << " never returned from accept()";
}

// Stops an acceptor thread when the surrounding case ends, however it ends.
//
// `close(listener)` deliberately does not release a parked `accept()` (see
// `BlockedAcceptSurvivesListenerClose`), so cleanup cannot wait for the
// descriptor to be closed: a worker that is still inside `accept()` is released
// with `pthread_cancel()`, whose signal makes the blocked syscall return EINTR.
//
// Declare the guard *after* the `AcceptorArgs` it guards: locals are destroyed in
// reverse order, so a failing `ASSERT_*` stops the worker while the arguments are
// still alive. A worker that does not react to cancellation is reported, and its
// arguments are left untouched; freeing them under a running thread would be the
// use-after-free this guard exists to prevent.
class AcceptorGuard {
public:
    AcceptorGuard() = default;
    AcceptorGuard(pthread_t thread, const AcceptorArgs& args) { Arm(thread, args); }
    ~AcceptorGuard() { Stop(); }

    AcceptorGuard(const AcceptorGuard&) = delete;
    AcceptorGuard& operator=(const AcceptorGuard&) = delete;

    void Arm(pthread_t thread, const AcceptorArgs& args) {
        thread_ = thread;
        args_ = &args;
        armed_ = true;
    }

private:
    void Stop() {
        if (!armed_) return;
        armed_ = false;

        // Only a worker that is still inside its loop needs a signal; skipping
        // the cancellation for one that already finished keeps the passing path
        // independent of signal delivery.
        if (!args_->finished.load()) {
            pthread_cancel(thread_);
            if (!WaitAcceptorFinished(*args_)) {
                // `pthread_join()` would block forever on a worker that never
                // reaches an interruption point, so report it and leave the
                // thread (and its arguments) alone.
                ADD_FAILURE() << "acceptor thread survived pthread_cancel";
                return;
            }
        }
        pthread_join(thread_, nullptr);
    }

    pthread_t thread_ = 0;
    const AcceptorArgs* args_ = nullptr;
    bool armed_ = false;
};

// ---------------------------------------------------------------------------
// Connector children
// ---------------------------------------------------------------------------

struct ConnectOutcome {
    int status;  // 0 on success, -1 on failure
    int error;   // errno when status == -1
};

// Owns one connector child and its pipes.
//
// The destructor reaps the child, so every path out of a case — including an
// early return after a failing `ASSERT_*` — leaves neither a running child nor a
// zombie behind (see the ground rules at the top of this file). Ownership is
// unique, hence move-only: `Reap()` and `FinishConnector()` clear the stored pid,
// which makes a later destruction a no-op.
struct ConnectorHandle {
    ConnectorHandle() = default;
    ~ConnectorHandle() { Reap(); }

    ConnectorHandle(const ConnectorHandle&) = delete;
    ConnectorHandle& operator=(const ConnectorHandle&) = delete;
    ConnectorHandle(ConnectorHandle&& other) noexcept { MoveFrom(other); }
    ConnectorHandle& operator=(ConnectorHandle&& other) noexcept {
        if (this != &other) {
            Reap();
            MoveFrom(other);
        }
        return *this;
    }

    pid_t pid = -1;
    int result_fd = -1;   // read end of the outcome pipe
    int release_fd = -1;  // write end of the release pipe (closed to let it exit)

    void Reap() {
        if (pid > 0) {
            kill(pid, SIGKILL);
            while (waitpid(pid, nullptr, 0) < 0 && errno == EINTR) {
            }
            pid = -1;
        }
        if (result_fd >= 0) {
            close(result_fd);
            result_fd = -1;
        }
        if (release_fd >= 0) {
            close(release_fd);
            release_fd = -1;
        }
    }

private:
    void MoveFrom(ConnectorHandle& other) {
        pid = other.pid;
        result_fd = other.result_fd;
        release_fd = other.release_fd;
        other.pid = -1;
        other.result_fd = -1;
        other.release_fd = -1;
    }
};

// Fork a child that connects to `address`.
//
// `hold_open` keeps the connection (and the child) alive until the parent closes
// `release_fd`; otherwise the child reports, writes one payload byte and exits.
//
// The caller must close the listening descriptor *before* calling this: a forked
// child inherits the descriptor table, and an inherited listener copy would keep
// the socket alive, hiding the very regression this file guards.
ConnectorHandle SpawnConnector(const AbstractAddress& address, bool hold_open) {
    ConnectorHandle handle;
    int result[2] = {-1, -1};
    int release[2] = {-1, -1};
    if (pipe(result) != 0) return handle;
    if (hold_open && pipe(release) != 0) {
        close(result[0]);
        close(result[1]);
        return handle;
    }

    const pid_t pid = fork();
    if (pid < 0) {
        close(result[0]);
        close(result[1]);
        if (hold_open) {
            close(release[0]);
            close(release[1]);
        }
        return handle;
    }

    if (pid == 0) {
        close(result[0]);
        if (hold_open) close(release[1]);

        const int fd = socket(AF_UNIX, SOCK_STREAM, 0);
        if (fd < 0) _exit(2);
        if (connect(fd, reinterpret_cast<const sockaddr*>(&address.addr), address.len) != 0) {
            const ConnectOutcome failure{-1, errno};
            (void)!write(result[1], &failure, sizeof(failure));
            _exit(3);
        }

        const ConnectOutcome success{0, 0};
        if (write(result[1], &success, sizeof(success)) != sizeof(success)) _exit(4);
        if (write(fd, &kPayload, 1) != 1) _exit(5);

        if (hold_open) {
            // Park until the parent releases us, then exit cleanly so the peer
            // sees a normal EOF rather than a reset.
            //
            // The release is a *byte*, not the pipe's EOF: a connector forked
            // after this one inherits a copy of this pipe's write end, so EOF
            // would never arrive and the parent would block forever in
            // `waitpid()`.
            char byte = 0;
            while (true) {
                const ssize_t got = read(release[0], &byte, 1);
                if (got >= 0 || errno != EINTR) break;
            }
            close(release[0]);
        }
        close(fd);
        _exit(0);
    }

    close(result[1]);
    handle.pid = pid;
    handle.result_fd = result[0];
    if (hold_open) {
        close(release[0]);
        handle.release_fd = release[1];
    }
    return handle;
}

// Wait for a connector child to report its outcome, within a bound.
bool ReadOutcome(ConnectorHandle& handle, ConnectOutcome* outcome, int timeout_ms) {
    pollfd ready{handle.result_fd, POLLIN, 0};
    if (poll(&ready, 1, timeout_ms) != 1) return false;
    return read(handle.result_fd, outcome, sizeof(*outcome)) ==
           static_cast<ssize_t>(sizeof(*outcome));
}

// Wait (bounded) for *any* of the given connectors to report, against a single
// shared deadline, and return its index. Polling every live pipe at once keeps
// the wait independent of which connector happened to be at the head of the
// backlog queue. Returns -1 on timeout.
int WaitAnyConnectorOutcome(ConnectorHandle* handles, size_t count, ConnectOutcome* outcome,
                            int timeout_ms) {
    // `ASSERT_*` cannot be used here (non-void return), so the bound is reported
    // non-fatally; the fixed-size arrays below must never be overflowed.
    EXPECT_LE(count, kMaxWaitingConnectors);
    if (count > kMaxWaitingConnectors) return -1;
    pollfd fds[kMaxWaitingConnectors] = {};
    int indexes[kMaxWaitingConnectors] = {-1, -1};
    nfds_t nfds = 0;
    for (size_t i = 0; i < count; ++i) {
        if (handles[i].pid <= 0) continue;
        fds[nfds].fd = handles[i].result_fd;
        fds[nfds].events = POLLIN;
        indexes[nfds] = static_cast<int>(i);
        ++nfds;
    }
    if (nfds == 0) return -1;
    if (poll(fds, nfds, timeout_ms) <= 0) return -1;
    for (nfds_t i = 0; i < nfds; ++i) {
        if ((fds[i].revents & POLLIN) == 0) continue;
        if (read(handles[indexes[i]].result_fd, outcome, sizeof(*outcome)) !=
            static_cast<ssize_t>(sizeof(*outcome))) {
            continue;
        }
        return indexes[i];
    }
    return -1;
}

// Reap a child that has reported success (it either exited or is waiting for the
// release pipe).
void FinishConnector(ConnectorHandle& handle, bool expect_clean_exit) {
    if (handle.release_fd >= 0) {
        // Release the parked child with a byte; see `SpawnConnector` for why EOF
        // cannot be used to tell it apart from a sibling's leaked write end.
        const char byte = 0;
        (void)!write(handle.release_fd, &byte, 1);
        close(handle.release_fd);
        handle.release_fd = -1;
    }
    if (handle.result_fd >= 0) {
        close(handle.result_fd);
        handle.result_fd = -1;
    }
    if (handle.pid <= 0) return;

    int status = 0;
    if (waitpid(handle.pid, &status, 0) != handle.pid) {
        ADD_FAILURE() << "waitpid failed for connector";
    } else if (expect_clean_exit) {
        EXPECT_TRUE(WIFEXITED(status));
        EXPECT_EQ(0, WEXITSTATUS(status));
    }
    handle.pid = -1;
}

bool SendByte(int fd) {
    return write(fd, &kPayload, 1) == 1;
}

void ExpectOneByte(int fd) {
    char byte = 0;
    EXPECT_EQ(1, read(fd, &byte, 1));
    EXPECT_EQ(kPayload, byte);
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

std::atomic<int> g_sigusr1_hits{0};

extern "C" void CountSigusr1(int) {
    g_sigusr1_hits.fetch_add(1);
}

// Install the SIGUSR1 handler for the calling process. An `ASSERT_*` in a void
// helper would only return from the helper and let the case pass without the
// signal being deliverable, so the failure is reported to the caller instead.
bool InstallSigusr1Handler() {
    struct sigaction action{};
    action.sa_handler = CountSigusr1;
    sigemptyset(&action.sa_mask);
    // No SA_RESTART: the whole point is that accept() must return EINTR.
    action.sa_flags = 0;
    return sigaction(SIGUSR1, &action, nullptr) == 0;
}

// `close(listener)` must not destroy the socket underneath a blocked accept():
// the open file description is kept alive for the duration of the syscall, so a
// connection arriving afterwards is still accepted (Linux
// `__sys_accept4()` + `fdget()`/`fdput()`). It must also not wake
// the blocked acceptor at all (`unix_release_sock()` never touches `sk_sleep`).
TEST(UnixAcceptClose, BlockedAcceptSurvivesListenerClose) {
    const AbstractAddress address = MakeAbstractAddress("survive");
    ScopedFd listener(MakeListener(address, 1));
    ASSERT_GE(listener.get(), 0) << strerror(errno);

    AcceptorArgs args;
    args.listener = listener.get();
    args.want = 1;
    pthread_t thread{};
    ASSERT_EQ(0, StartAcceptor(&args, &thread));
    // Declared after `args`: on an early return the guard stops the worker while
    // the arguments are still alive.
    AcceptorGuard guard(thread, args);
    const pid_t tid = AcceptorTid(args);
    ASSERT_GT(tid, 0);
    if (!WaitThreadSleeping(tid)) {
        FAIL() << "acceptor never blocked in accept";
        return;
    }

    // Close the only descriptor for the listener. No other process or thread
    // holds a copy, so this would run `do_close()` without the fix.
    listener.reset();

    // Negative assertion: `close()` must not wake a blocked accept.
    usleep(200 * 1000);
    if (!WaitThreadSleeping(tid, 1)) {
        FAIL() << "close(listener) must not wake a blocked accept";
        return;
    }

    // The connector is forked only now, so it cannot inherit the listener.
    ConnectorHandle connector = SpawnConnector(address, /*hold_open=*/true);
    ASSERT_GT(connector.pid, 0);

    ConnectOutcome outcome{};
    ASSERT_TRUE(ReadOutcome(connector, &outcome, kResultWaitMs))
        << "the connection was never accepted after close(listener)";
    EXPECT_EQ(0, outcome.status) << "connect failed with errno " << outcome.error;

    ExpectAcceptorExited(args, "accept after close(listener)");
    ASSERT_EQ(1, args.count.load());
    ScopedFd accepted(args.fds[0].load());
    ASSERT_GE(accepted.get(), 0);

    ExpectOneByte(accepted.get());
    EXPECT_TRUE(SendByte(accepted.get()));

    FinishConnector(connector, /*expect_clean_exit=*/true);
}

// A signalled blocked accept must report EINTR. Before the lifetime fix the
// wake-up re-evaluated the accept predicate on a torn-down socket and panicked.
TEST(UnixAcceptClose, BlockedAcceptReturnsEintrAfterCloseAndSignal) {
    ASSERT_TRUE(InstallSigusr1Handler()) << strerror(errno);

    const AbstractAddress address = MakeAbstractAddress("eintr");
    ScopedFd listener(MakeListener(address, 1));
    ASSERT_GE(listener.get(), 0) << strerror(errno);

    AcceptorArgs args;
    args.listener = listener.get();
    args.want = 1;
    pthread_t thread{};
    ASSERT_EQ(0, StartAcceptor(&args, &thread));
    AcceptorGuard guard(thread, args);
    const pid_t tid = AcceptorTid(args);
    ASSERT_GT(tid, 0);
    if (!WaitThreadSleeping(tid)) {
        FAIL() << "acceptor never blocked in accept";
        return;
    }

    listener.reset();
    usleep(200 * 1000);

    // Thread-directed signal: a process-directed one may be taken by another
    // thread and would then never interrupt the blocked accept.
    ASSERT_EQ(0, pthread_kill(thread, SIGUSR1));

    ExpectAcceptorExited(args, "signalled accept");
    EXPECT_EQ(0, args.count.load());
    EXPECT_EQ(-1, args.fds[0].load());
    EXPECT_EQ(EINTR, args.last_errno.load())
        << "blocked accept must report EINTR, got errno " << args.last_errno.load();
    EXPECT_GE(g_sigusr1_hits.load(), 1) << "SIGUSR1 handler never ran";
}

// `shutdown(listener, SHUT_RD)` is the only legal release for a blocked accept
// (Linux `unix_shutdown()` in `net/unix/af_unix.c`): it latches
// RCV_SHUTDOWN, wakes the socket's own waiters, and turns the accept into EINVAL.
TEST(UnixAcceptClose, ListenerShutdownReadReleasesBlockedAccept) {
    const AbstractAddress address = MakeAbstractAddress("shutrd");
    ScopedFd listener(MakeListener(address, 1));
    ASSERT_GE(listener.get(), 0) << strerror(errno);

    AcceptorArgs blocked;
    blocked.listener = listener.get();
    blocked.want = 1;
    pthread_t blocked_thread{};
    ASSERT_EQ(0, StartAcceptor(&blocked, &blocked_thread));
    AcceptorGuard blocked_guard(blocked_thread, blocked);
    const pid_t tid = AcceptorTid(blocked);
    ASSERT_GT(tid, 0);
    if (!WaitThreadSleeping(tid)) {
        FAIL() << "acceptor never blocked in accept";
        return;
    }

    ASSERT_EQ(0, shutdown(listener.get(), SHUT_RD)) << strerror(errno);

    ExpectAcceptorExited(blocked, "accept released by SHUT_RD");
    EXPECT_EQ(0, blocked.count.load());
    EXPECT_EQ(EINVAL, blocked.last_errno.load())
        << "blocked accept must fail with EINVAL after SHUT_RD";

    // A fresh blocking accept must not sleep either.
    AcceptorArgs second;
    second.listener = listener.get();
    second.want = 1;
    pthread_t second_thread{};
    ASSERT_EQ(0, StartAcceptor(&second, &second_thread));
    AcceptorGuard second_guard(second_thread, second);
    ExpectAcceptorExited(second, "accept after SHUT_RD");
    EXPECT_EQ(0, second.count.load());
    EXPECT_EQ(EINVAL, second.last_errno.load());

    // A non-blocking accept reports EAGAIN instead: with `timeo == 0` Linux never
    // reaches the RCV_SHUTDOWN check (`__skb_wait_for_more_packets()`).
    const int flags = fcntl(listener.get(), F_GETFL, 0);
    ASSERT_GE(flags, 0);
    ASSERT_EQ(0, fcntl(listener.get(), F_SETFL, flags | O_NONBLOCK));
    errno = 0;
    EXPECT_EQ(-1, accept(listener.get(), nullptr, nullptr));
    EXPECT_EQ(EAGAIN, errno);

    // New connections are refused, with no wait.
    ScopedFd client(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(client.get(), 0);
    ASSERT_EQ(0, fcntl(client.get(), F_SETFL, O_NONBLOCK));
    errno = 0;
    EXPECT_EQ(-1, connect(client.get(), reinterpret_cast<const sockaddr*>(&address.addr),
                          address.len));
    EXPECT_EQ(ECONNREFUSED, errno);

    // SHUT_WR has no observable effect on a listener.
    EXPECT_EQ(0, shutdown(listener.get(), SHUT_WR)) << strerror(errno);
}

// Queued connections win over a latched receive shutdown: the data is handed out
// before the terminal state is observed (Linux `unix_accept()` dequeues first).
TEST(UnixAcceptClose, ListenerShutdownReadStillAcceptsQueuedConnection) {
    const AbstractAddress address = MakeAbstractAddress("queued");
    ScopedFd listener(MakeListener(address, 1));
    ASSERT_GE(listener.get(), 0) << strerror(errno);

    ScopedFd client(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(client.get(), 0);
    ASSERT_EQ(0, connect(client.get(), reinterpret_cast<const sockaddr*>(&address.addr),
                         address.len))
        << strerror(errno);
    ASSERT_TRUE(SendByte(client.get()));

    ASSERT_EQ(0, shutdown(listener.get(), SHUT_RD)) << strerror(errno);

    ScopedFd accepted(accept(listener.get(), nullptr, nullptr));
    ASSERT_GE(accepted.get(), 0) << "queued connection must survive SHUT_RD: " << strerror(errno);
    ExpectOneByte(accepted.get());

    // With the queue drained the shutdown is now visible.
    AcceptorArgs next;
    next.listener = listener.get();
    next.want = 1;
    pthread_t thread{};
    ASSERT_EQ(0, StartAcceptor(&next, &thread));
    AcceptorGuard guard(thread, next);
    ExpectAcceptorExited(next, "accept once the queue is drained");
    EXPECT_EQ(0, next.count.load());
    EXPECT_EQ(EINVAL, next.last_errno.load());
}

// One freed backlog slot must advance exactly one blocked connector; the others
// stay parked. This is the property `BacklogRetry` exists to preserve.
TEST(UnixAcceptClose, FreeingOneSlotAdvancesOneConnector) {
    const AbstractAddress address = MakeAbstractAddress("slot");
    ScopedFd listener(MakeListener(address, /*backlog=*/0));
    ASSERT_GE(listener.get(), 0) << strerror(errno);

    // Occupy the single slot (backlog 0 still admits one pending connection).
    ConnectorHandle first = SpawnConnector(address, /*hold_open=*/false);
    ASSERT_GT(first.pid, 0);
    ConnectOutcome outcome{};
    ASSERT_TRUE(ReadOutcome(first, &outcome, kResultWaitMs));
    ASSERT_EQ(0, outcome.status) << "first connect failed with errno " << outcome.error;
    FinishConnector(first, /*expect_clean_exit=*/true);

    ConnectorHandle waiting[2];
    waiting[0] = SpawnConnector(address, /*hold_open=*/false);
    waiting[1] = SpawnConnector(address, /*hold_open=*/false);
    ASSERT_GT(waiting[0].pid, 0);
    ASSERT_GT(waiting[1].pid, 0);
    for (auto& handle : waiting) {
        if (!WaitProcessSleeping(handle.pid)) {
            FAIL() << "connector never blocked on the full backlog";
            return;
        }
    }

    for (int step = 0; step < 2; ++step) {
        ScopedFd accepted(accept(listener.get(), nullptr, nullptr));
        ASSERT_GE(accepted.get(), 0) << strerror(errno);

        // Exactly the connector that owns the freed slot must be admitted. The
        // bound is the full result wait, so a loaded machine cannot turn a
        // correct kernel into a spurious failure.
        ConnectOutcome result{};
        const int advanced = WaitAnyConnectorOutcome(waiting, 2, &result, kResultWaitMs);
        ASSERT_GE(advanced, 0) << "a freed backlog slot advanced no connector";
        EXPECT_EQ(0, result.status) << "connect failed with errno " << result.error;
        FinishConnector(waiting[advanced], /*expect_clean_exit=*/true);

        // The second connector must *not* be admitted: it stays queued until the
        // next slot is freed. The short grace only bounds a false pass; the
        // queue-ownership check below is the real assertion.
        if (WaitAnyConnectorOutcome(waiting, 2, &result, kSecondConnectorGraceMs) >= 0) {
            FAIL() << "one freed slot admitted a second connector";
            return;
        }

        // Everyone still waiting must be parked again, not spinning or admitted.
        for (auto& handle : waiting) {
            if (handle.pid <= 0) continue;
            ASSERT_TRUE(WaitProcessSleeping(handle.pid))
                << "connector left the backlog wait without being admitted";
        }
    }

}

// `shutdown(listener, SHUT_RD)` must wake an epoll subscriber *and* report the
// latched receive shutdown as readable; a full `shutdown(listener, SHUT_RDWR)`
// must additionally report `EPOLLHUP`. Linux `unix_poll()` (`net/unix/af_unix.c`)
// derives those bits from `sk_shutdown`: `RCV_SHUTDOWN` adds
// `EPOLLIN|EPOLLRDNORM|EPOLLRDHUP`, and only the full mask adds `EPOLLHUP`.
//
// The first wait blocks in `epoll_wait()` on purpose: reading readiness without
// sleeping would pass even if `shutdown()` forgot to call `wakeup_epoll()`.
TEST(UnixAcceptClose, ListenerShutdownReadReportsPollEvents) {
    const AbstractAddress address = MakeAbstractAddress("poll");
    ScopedFd listener(MakeListener(address, /*backlog=*/0));
    ASSERT_GE(listener.get(), 0) << strerror(errno);

    ScopedFd epfd(epoll_create1(EPOLL_CLOEXEC));
    ASSERT_GE(epfd.get(), 0) << strerror(errno);
    epoll_event interest{};
    interest.events = EPOLLIN | EPOLLRDHUP;
    ASSERT_EQ(0, epoll_ctl(epfd.get(), EPOLL_CTL_ADD, listener.get(), &interest)) << strerror(errno);

    epoll_event event{};
    EXPECT_EQ(0, epoll_wait(epfd.get(), &event, 1, /*timeout_ms=*/0))
        << "an idle listener must not be readable";

    ASSERT_EQ(0, shutdown(listener.get(), SHUT_RD)) << strerror(errno);

    ASSERT_EQ(1, epoll_wait(epfd.get(), &event, 1, kResultWaitMs))
        << "shutdown(SHUT_RD) must wake a blocked epoll_wait: " << strerror(errno);
    EXPECT_NE(0u, event.events & EPOLLIN);
    EXPECT_NE(0u, event.events & EPOLLRDHUP) << "a receive shutdown is a half-close";
    EXPECT_EQ(0u, event.events & EPOLLHUP)
        << "a receive-only shutdown must not report EPOLLHUP";

    ASSERT_EQ(0, shutdown(listener.get(), SHUT_WR)) << strerror(errno);
    ASSERT_EQ(1, epoll_wait(epfd.get(), &event, 1, kResultWaitMs)) << strerror(errno);
    EXPECT_NE(0u, event.events & EPOLLHUP) << "SHUT_RDWR must report EPOLLHUP";
}

// Multiple blocked acceptors and blocked connectors must all make progress: the
// two waiter classes live on separate queues and must not cross-wake or starve.
TEST(UnixAcceptClose, MultipleAcceptorsAndConnectorsMakeProgress) {
    const int kConnectors = 4;
    const AbstractAddress address = MakeAbstractAddress("many");
    ScopedFd listener(MakeListener(address, /*backlog=*/1));
    ASSERT_GE(listener.get(), 0) << strerror(errno);

    AcceptorArgs acceptors[2];
    pthread_t threads[2] = {};
    // Declared after `acceptors`: both workers are stopped while the arguments
    // they read are still alive.
    AcceptorGuard guards[2];
    for (int i = 0; i < 2; ++i) {
        acceptors[i].listener = listener.get();
        acceptors[i].want = kConnectors / 2;
        ASSERT_EQ(0, StartAcceptor(&acceptors[i], &threads[i]));
        guards[i].Arm(threads[i], acceptors[i]);
    }

    ConnectorHandle connectors[kConnectors];
    for (auto& handle : connectors) {
        handle = SpawnConnector(address, /*hold_open=*/true);
        ASSERT_GT(handle.pid, 0);
    }

    for (auto& handle : connectors) {
        ConnectOutcome outcome{};
        ASSERT_TRUE(ReadOutcome(handle, &outcome, kResultWaitMs))
            << "connector starved behind acceptors";
        EXPECT_EQ(0, outcome.status) << "connect failed with errno " << outcome.error;
    }

    for (int i = 0; i < 2; ++i) {
        ExpectAcceptorExited(acceptors[i], "accept for one connector");
    }

    int accepted_total = 0;
    for (auto& acceptor : acceptors) accepted_total += acceptor.count.load();
    EXPECT_EQ(kConnectors, accepted_total);

    for (auto& acceptor : acceptors) {
        for (auto& fd : acceptor.fds) {
            const int value = fd.load();
            if (value < 0) continue;
            ScopedFd accepted(value);
            ExpectOneByte(accepted.get());
        }
    }

    for (auto& handle : connectors) FinishConnector(handle, /*expect_clean_exit=*/true);
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
