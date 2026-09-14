// Thread-level semantics of /proc/<pid>/task/<tid>/status (issue #2269).
//
// The thread directory used to expose only stat/mem/ns/oom_score_adj, so
// /proc/<pid>/task/<tid>/status returned ENOENT. These cases pin the fixed
// behaviour and guard the pre-existing /proc/<pid>/status semantics.
//
//   1. the leader's thread view equals the process view;
//   2. a non-leader thread is described by its own Name/State/Pid, with Tgid
//      still naming the thread group;
//   3. once a thread is gone, the path must not serve another task.
//
// Pid/Tgid scoping by pid namespace is provided by ProcPidTarget and is not
// covered here: a procfs mounted inside a new pid namespace does not let this
// suite address its threads.

#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

#include <algorithm>
#include <chrono>
#include <map>
#include <string>
#include <vector>

namespace {

using Clock = std::chrono::steady_clock;

constexpr int kPollTimeoutMs = 2000;

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

class UniqueFd {
public:
    UniqueFd() = default;
    explicit UniqueFd(int fd) : fd_(fd) {}
    ~UniqueFd() { Reset(); }

    UniqueFd(const UniqueFd&) = delete;
    UniqueFd& operator=(const UniqueFd&) = delete;

    int get() const { return fd_; }
    bool valid() const { return fd_ >= 0; }

    void Reset(int fd = -1) {
        if (fd_ >= 0) {
            close(fd_);
        }
        fd_ = fd;
    }

private:
    int fd_ = -1;
};

// Procfs files are regenerated on every read_at() while the file position stays
// a plain byte offset, so a second read() at a stale offset can hand back tail
// bytes of a longer re-render. One read(2), with a buffer far larger than these
// files, therefore yields exactly one coherent snapshot; callers that need the
// whole file (and every compared field is checked for presence) would notice if
// the content ever outgrew the buffer.
constexpr size_t kSnapshotBufSize = 4096;

bool ReadProcSnapshot(const std::string& path, std::string* out, int* err_out) {
    UniqueFd fd(open(path.c_str(), O_RDONLY));
    if (!fd.valid()) {
        *err_out = errno;
        return false;
    }
    char buf[kSnapshotBufSize];
    for (;;) {
        const ssize_t n = read(fd.get(), buf, sizeof(buf));
        if (n >= 0) {
            out->assign(buf, static_cast<size_t>(n));
            return true;
        }
        if (errno != EINTR) {
            *err_out = errno;
            return false;
        }
    }
}

// Escape non-printable bytes so failures show the exact file contents.
std::string Escape(const std::string& s) {
    std::string out;
    out.reserve(s.size() + 16);
    for (char c : s) {
        switch (c) {
            case '\t':
                out += "\\t";
                break;
            case '\n':
                out += "\\n";
                break;
            case '\0':
                out += "\\0";
                break;
            default:
                if (static_cast<unsigned char>(c) < 0x20 || static_cast<unsigned char>(c) >= 0x7f) {
                    char hex[8];
                    snprintf(hex, sizeof(hex), "\\x%02x", static_cast<unsigned char>(c));
                    out += hex;
                } else {
                    out.push_back(c);
                }
        }
    }
    return out;
}

std::string Trim(std::string s) {
    auto not_space = [](unsigned char c) {
        return c != ' ' && c != '\t' && c != '\n' && c != '\r' && c != '\0';
    };
    s.erase(s.begin(), std::find_if(s.begin(), s.end(), not_space));
    s.erase(std::find_if(s.rbegin(), s.rend(), not_space).base(), s.end());
    return s;
}

// Parse the "Key:\tValue" lines; the kernel appends one NUL via trim_string().
std::map<std::string, std::string> ParseStatus(const std::string& text) {
    std::map<std::string, std::string> fields;
    size_t pos = 0;
    while (pos <= text.size()) {
        const size_t nl = text.find('\n', pos);
        std::string line = (nl == std::string::npos) ? text.substr(pos) : text.substr(pos, nl - pos);
        pos = (nl == std::string::npos) ? text.size() + 1 : nl + 1;

        std::string clean;
        clean.reserve(line.size());
        for (char c : line) {
            if (c != '\0') {
                clean.push_back(c);
            }
        }
        const size_t colon = clean.find(':');
        if (colon == std::string::npos) {
            continue;
        }
        fields[clean.substr(0, colon)] = Trim(clean.substr(colon + 1));
    }
    return fields;
}

std::string Field(const std::map<std::string, std::string>& fields, const char* key) {
    const auto it = fields.find(key);
    return it == fields.end() ? std::string() : it->second;
}

std::string FormatFields(const std::map<std::string, std::string>& fields) {
    std::string all;
    for (const auto& entry : fields) {
        all += entry.first + "=" + entry.second + " ";
    }
    return all;
}

std::string TaskStatusPath(pid_t pid, long tid) {
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/task/%ld/status", pid, tid);
    return std::string(path);
}

std::string ProcessStatusPath(pid_t pid) {
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/status", pid);
    return std::string(path);
}

std::string TaskStatPath(pid_t pid, long tid) {
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/task/%ld/stat", pid, tid);
    return std::string(path);
}

// Third field of /proc/<pid>/stat, i.e. the single Linux-style state character.
char ParseStatState(const std::string& text) {
    const size_t close_paren = text.rfind(')');
    if (close_paren == std::string::npos || close_paren + 2 >= text.size()) {
        return '\0';
    }
    return text[close_paren + 2];
}

bool WaitForStatState(pid_t pid, long tid, char expected) {
    const std::string path = TaskStatPath(pid, tid);
    const Clock::time_point deadline = Clock::now() + std::chrono::milliseconds(kPollTimeoutMs);
    for (;;) {
        std::string text;
        int err = 0;
        if (ReadProcSnapshot(path, &text, &err) && ParseStatState(text) == expected) {
            return true;
        }
        if (Clock::now() >= deadline) {
            return false;
        }
        usleep(5000);
    }
}

// Bounded poll until `path` can no longer be opened; the errno is returned
// through *err_out so the caller can require exactly the expected one.
bool WaitForOpenFailure(const std::string& path, int* err_out) {
    const Clock::time_point deadline = Clock::now() + std::chrono::milliseconds(kPollTimeoutMs);
    for (;;) {
        UniqueFd fd(open(path.c_str(), O_RDONLY));
        if (!fd.valid()) {
            *err_out = errno;
            return true;
        }
        if (Clock::now() >= deadline) {
            return false;
        }
        usleep(5000);
    }
}

long GetTid() {
    return static_cast<long>(syscall(SYS_gettid));
}

// ---------------------------------------------------------------------------
// Worker thread: publishes its tid, then blocks in a pipe read until the owner
// releases it. The destructor always joins, so a failing ASSERT cannot leave a
// blocked thread (or a dangling `this`) behind for the next test case.
// ---------------------------------------------------------------------------

class Worker {
public:
    Worker() = default;
    ~Worker() { Stop(); }
    Worker(const Worker&) = delete;
    Worker& operator=(const Worker&) = delete;

    bool Start();
    void Stop();
    long tid() const { return tid_; }

private:
    static void* Main(void* arg);

    static void ClosePipe(int fds[2]) {
        for (int i = 0; i < 2; ++i) {
            if (fds[i] >= 0) {
                close(fds[i]);
                fds[i] = -1;
            }
        }
    }

    int ready_pipe_[2] = {-1, -1};
    int release_pipe_[2] = {-1, -1};
    long tid_ = 0;
    pthread_t thread_ = {};
    bool started_ = false;
};

void* Worker::Main(void* arg) {
    Worker* self = static_cast<Worker*>(arg);
    prctl(PR_SET_NAME, "worker", 0, 0, 0);
    self->tid_ = GetTid();

    const char ready = 'r';
    if (write(self->ready_pipe_[1], &ready, 1) != 1) {
        return nullptr;
    }

    char buf[8];
    while (read(self->release_pipe_[0], buf, sizeof(buf)) > 0) {
    }
    return nullptr;
}

bool Worker::Start() {
    if (pipe(ready_pipe_) != 0) {
        return false;
    }
    if (pipe(release_pipe_) != 0) {
        ClosePipe(ready_pipe_);
        return false;
    }
    if (pthread_create(&thread_, nullptr, &Worker::Main, this) != 0) {
        ClosePipe(release_pipe_);
        ClosePipe(ready_pipe_);
        return false;
    }
    started_ = true;

    // The pipe write orders the worker's `tid_` store before this read.
    char byte = 0;
    ssize_t n = 0;
    do {
        n = read(ready_pipe_[0], &byte, 1);
    } while (n < 0 && errno == EINTR);
    if (n != 1) {
        Stop();
        return false;
    }
    return true;
}

void Worker::Stop() {
    // Closing the release pipe makes the worker's blocking read() return 0.
    ClosePipe(release_pipe_);
    if (started_) {
        pthread_join(thread_, nullptr);
        started_ = false;
    }
    ClosePipe(ready_pipe_);
}

// Fields that two independent snapshots of the same task must agree on. The
// rest of what status.rs emits is deliberately absent: State/Time/Stime/vrtime/
// cpu_id advance with scheduling, flags carries transient bits (NEED_RSEQ), and
// the Vm* values belong to mm and can move while the test itself allocates.
// Every listed field is also required to be present, so a field that disappears
// from status.rs cannot silently turn this into a no-op comparison.
const char* const kStableStatusFields[] = {
    "Name",   "Pid",    "Tgid",    "Ppid",     "TracerPid",
    "FDSize", "Tty",    "Kthread", "priority", "NoNewPrivs",
    "Seccomp", "Seccomp_filters",
};

}  // namespace

// ---------------------------------------------------------------------------
// Case 1: the leader's thread view matches the process view
// ---------------------------------------------------------------------------
TEST(ProcTaskStatus, LeaderStatusMatchesProcessStatus) {
    const pid_t pid = getpid();
    const long tid = GetTid();
    ASSERT_EQ(static_cast<long>(pid), tid) << "the main thread must have tid == pid";

    std::string task_text;
    int err = 0;
    ASSERT_TRUE(ReadProcSnapshot(TaskStatusPath(pid, tid), &task_text, &err))
        << "cannot read " << TaskStatusPath(pid, tid) << ": errno=" << err << " ("
        << strerror(err) << ")";

    std::string proc_text;
    ASSERT_TRUE(ReadProcSnapshot(ProcessStatusPath(pid), &proc_text, &err))
        << "cannot read " << ProcessStatusPath(pid) << ": errno=" << err << " ("
        << strerror(err) << ")";

    const auto task_fields = ParseStatus(task_text);
    const auto proc_fields = ParseStatus(proc_text);
    const std::string dump =
        std::string("\ntask raw: ") + Escape(task_text) + "\nproc raw: " + Escape(proc_text);

    // Identical field sets: the thread view must not omit or add anything.
    std::vector<std::string> task_keys;
    std::vector<std::string> proc_keys;
    for (const auto& entry : task_fields) {
        task_keys.push_back(entry.first);
    }
    for (const auto& entry : proc_fields) {
        proc_keys.push_back(entry.first);
    }
    EXPECT_EQ(task_keys, proc_keys) << "task fields: " << FormatFields(task_fields)
                                    << "\nproc fields: " << FormatFields(proc_fields) << dump;

    for (const char* key : kStableStatusFields) {
        const bool present = task_fields.count(key) == 1u;
        EXPECT_TRUE(present) << "field " << key << " missing from the thread view" << dump;
        if (!present) {
            continue;
        }
        EXPECT_EQ(Field(task_fields, key), Field(proc_fields, key))
            << "field " << key << " differs between the thread and process views" << dump;
    }

    EXPECT_EQ(std::to_string(pid), Field(task_fields, "Pid"));
    EXPECT_EQ(std::to_string(pid), Field(task_fields, "Tgid"));
}

// ---------------------------------------------------------------------------
// Case 2: a non-leader thread is described by its own fields
// ---------------------------------------------------------------------------
TEST(ProcTaskStatus, NonLeaderThreadStatusDescribesThread) {
    const pid_t pid = getpid();
    const long main_tid = GetTid();

    // Use the process view as the reference for the leader's comm instead of
    // PR_GET_NAME, whose 16-byte buffer has a narrower contract.
    std::string proc_before;
    int err = 0;
    ASSERT_TRUE(ReadProcSnapshot(ProcessStatusPath(pid), &proc_before, &err)) << "errno=" << err;
    const std::string main_comm = Field(ParseStatus(proc_before), "Name");
    ASSERT_FALSE(main_comm.empty()) << Escape(proc_before);

    Worker worker;
    ASSERT_TRUE(worker.Start());
    ASSERT_GT(worker.tid(), 0L);
    ASSERT_NE(worker.tid(), main_tid);

    // Wait until the worker really is in an interruptible block, so that the
    // assertions below describe this thread and not a transient state.
    const std::string stat_path = TaskStatPath(pid, worker.tid());
    ASSERT_TRUE(WaitForStatState(pid, worker.tid(), 'S'))
        << "worker thread never reached an interruptible block: " << stat_path;

    std::string text;
    ASSERT_TRUE(ReadProcSnapshot(TaskStatusPath(pid, worker.tid()), &text, &err))
        << "cannot read " << TaskStatusPath(pid, worker.tid()) << ": errno=" << err << " ("
        << strerror(err) << ")";

    const auto fields = ParseStatus(text);
    EXPECT_EQ("worker", Field(fields, "Name")) << Escape(text);
    EXPECT_EQ(std::to_string(worker.tid()), Field(fields, "Pid"));
    EXPECT_EQ(std::to_string(pid), Field(fields, "Tgid"));
    EXPECT_NE("0", Field(fields, "Tgid"));
    // A thread inherits its creator's parent, so this must agree with the
    // process view while the parent is alive. Reparenting only rewrites the
    // group leader today, which is a separate pre-existing gap.
    EXPECT_EQ(Field(ParseStatus(proc_before), "Ppid"), Field(fields, "Ppid"));

    // The thread state must come from the worker (blocked), not from the leader.
    std::string proc_after;
    ASSERT_TRUE(ReadProcSnapshot(ProcessStatusPath(pid), &proc_after, &err)) << "errno=" << err;
    const auto proc_fields = ParseStatus(proc_after);
    EXPECT_NE(Field(proc_fields, "State"), Field(fields, "State"))
        << "the thread state must not be copied from the thread group leader";

    // Reverse direction: naming a thread must not rename the process.
    EXPECT_EQ(main_comm, Field(proc_fields, "Name"));
    EXPECT_NE(main_comm, Field(fields, "Name"));
}

// ---------------------------------------------------------------------------
// Case 3: a thread that is gone must not serve another task
// ---------------------------------------------------------------------------
TEST(ProcTaskStatus, ExitedThreadStatusIsNotServed) {
    const pid_t pid = getpid();

    Worker worker;
    ASSERT_TRUE(worker.Start());
    ASSERT_GT(worker.tid(), 0L);

    const std::string path = TaskStatusPath(pid, worker.tid());

    // Hold an fd obtained while the thread was alive, then let the thread exit.
    UniqueFd held(open(path.c_str(), O_RDONLY));
    ASSERT_TRUE(held.valid()) << "cannot open " << path << ": errno=" << errno << " ("
                              << strerror(errno) << ")";

    worker.Stop();

    // The stale fd may still report the exiting thread's own final state, but it
    // must never resolve to a different task; once the pid link is gone it must
    // fail with ESRCH.
    char buf[kSnapshotBufSize];
    errno = 0;
    const ssize_t n = read(held.get(), buf, sizeof(buf));
    if (n < 0) {
        EXPECT_EQ(ESRCH, errno) << "unexpected errno for a stale thread status fd";
    } else {
        const auto fields = ParseStatus(std::string(buf, static_cast<size_t>(n)));
        EXPECT_EQ(std::to_string(worker.tid()), Field(fields, "Pid"))
            << "stale thread status fd served another task";
        EXPECT_EQ(std::to_string(pid), Field(fields, "Tgid"))
            << "stale thread status fd served another thread group";
    }
    held.Reset();

    // Path lookup: once the thread is reclaimed the path must be ENOENT rather
    // than resolving to some other task.
    int open_errno = 0;
    ASSERT_TRUE(WaitForOpenFailure(path, &open_errno))
        << "the thread path still resolves after the thread exited: " << path;
    EXPECT_EQ(ENOENT, open_errno) << "unexpected errno for an exited thread: "
                                  << strerror(open_errno);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
