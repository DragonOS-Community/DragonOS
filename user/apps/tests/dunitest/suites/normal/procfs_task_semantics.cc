// procfs task semantics (issue #2283).
//
// Three behaviours are pinned here, each against the Linux 6.6 model:
//
//   1. one fd sees one record. Linux serves these files through
//      single_open()/seq_read_iter(), so a read() that reached EOF keeps
//      returning 0 even while the record grows, and moving the file position
//      re-renders. Before the fix every read() re-rendered and the stale byte
//      offset sliced the *new* render, so a second read() could hand back tail
//      bytes of a longer record;
//   2. re-parenting a thread group rewrites the parent links of every thread,
//      so /proc/<pid>/task/<tid>/status and /proc/<pid>/status agree on Ppid;
//   3. /proc/<nr> resolves any task that still holds a PID link (Linux
//      proc_pid_lookup() -> find_task_by_pid_ns()), while /proc *lists* group
//      leaders only (Linux next_tgid()).
//
// Companion analysis:
//   docs/kernel/filesystem/proc/procfs-task-semantics-root-cause.md
//   docs/kernel/filesystem/proc/procfs-task-semantics-fix-plan.md

#include <gtest/gtest.h>

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <map>
#include <set>
#include <string>
#include <vector>

namespace {

constexpr size_t kReadChunk = 128;
constexpr int kPollTimeoutMs = 2000;

/// Argument that turns the test binary into the exec()ed helper of
/// MapsStreamKeepsTheAddressSpaceOpenedOn: it maps a marker region, reports its
/// address on the inherited pipe and parks, so the parent can watch the target
/// change address spaces while an fd streams /proc/<pid>/maps.
constexpr char kMapsExecParkArg[] = "procfs_task_semantics_maps_exec_park";

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

std::string Escape(const std::string& s) {
    std::string out;
    out.reserve(s.size() + 16);
    for (char c : s) {
        if (c == '\n') {
            out += "\\n";
        } else if (c == '\t') {
            out += "\\t";
        } else if (static_cast<unsigned char>(c) < 0x20) {
            char hex[8];
            snprintf(hex, sizeof(hex), "\\x%02x", static_cast<unsigned char>(c));
            out += hex;
        } else {
            out.push_back(c);
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

// Read `fd` until EOF in `chunk`-sized pieces, appending the bytes to `out`.
// Returns the errno of a failed read, or 0 on success.
int ReadToEof(int fd, size_t chunk, std::string* out) {
    char buf[256];
    if (chunk > sizeof(buf)) {
        chunk = sizeof(buf);
    }
    for (;;) {
        const ssize_t n = read(fd, buf, chunk);
        if (n == 0) {
            return 0;
        }
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return errno;
        }
        out->append(buf, static_cast<size_t>(n));
    }
}

bool ReadWholePath(const std::string& path, std::string* out, int* err_out) {
    UniqueFd fd(open(path.c_str(), O_RDONLY));
    if (!fd.valid()) {
        *err_out = errno;
        return false;
    }
    *err_out = ReadToEof(fd.get(), kReadChunk, out);
    return *err_out == 0;
}

// Shape of a record: the number of whitespace-separated tokens on each line.
// Two renders of the same file agree on this even when the numeric values (and
// therefore the byte length) moved, which is what makes it usable as the
// structural half of the chunked-read check.
std::vector<size_t> Shape(const std::string& text) {
    std::vector<size_t> shape;
    size_t pos = 0;
    while (pos < text.size()) {
        size_t nl = text.find('\n', pos);
        if (nl == std::string::npos) {
            nl = text.size();
        }
        size_t tokens = 0;
        bool in_token = false;
        for (size_t i = pos; i < nl; ++i) {
            const char c = text[i];
            const bool space = (c == ' ' || c == '\t' || c == '\0' || c == '\r');
            if (!space && !in_token) {
                ++tokens;
            }
            in_token = !space;
        }
        shape.push_back(tokens);
        pos = nl + 1;
    }
    return shape;
}

std::map<std::string, std::string> ParseStatus(const std::string& text) {
    std::map<std::string, std::string> fields;
    size_t pos = 0;
    while (pos <= text.size()) {
        const size_t nl = text.find('\n', pos);
        std::string line = (nl == std::string::npos) ? text.substr(pos) : text.substr(pos, nl - pos);
        pos = (nl == std::string::npos) ? text.size() + 1 : nl + 1;

        std::string clean;
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

// Linux spells the field "PPid", DragonOS renders it as "Ppid"; both names must
// resolve to the same value so the assertions below describe the semantics and
// not the spelling.
std::string Field(const std::map<std::string, std::string>& fields, const char* key) {
    auto it = fields.find(key);
    if (it != fields.end()) {
        return it->second;
    }
    if (strcmp(key, "Ppid") == 0) {
        it = fields.find("PPid");
    } else if (strcmp(key, "PPid") == 0) {
        it = fields.find("Ppid");
    }
    return it == fields.end() ? std::string() : it->second;
}

std::string StatusPath(pid_t pid, const char* suffix = "status") {
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/%s", pid, suffix);
    return std::string(path);
}

std::string TaskStatusPath(pid_t pid, long tid, const char* suffix = "status") {
    char path[80];
    snprintf(path, sizeof(path), "/proc/%d/task/%ld/%s", pid, tid, suffix);
    return std::string(path);
}

std::string TidPath(long tid, const char* suffix = "") {
    char path[64];
    snprintf(path, sizeof(path), "/proc/%ld/%s", tid, suffix);
    return std::string(path);
}

long GetTid() {
    return static_cast<long>(syscall(SYS_gettid));
}

std::vector<std::string> ListDir(const std::string& path) {
    std::vector<std::string> names;
    DIR* d = opendir(path.c_str());
    if (d == nullptr) {
        return names;
    }
    while (struct dirent* e = readdir(d)) {
        if (strcmp(e->d_name, ".") == 0 || strcmp(e->d_name, "..") == 0) {
            continue;
        }
        names.push_back(e->d_name);
    }
    closedir(d);
    std::sort(names.begin(), names.end());
    return names;
}

bool Contains(const std::vector<std::string>& v, const std::string& needle) {
    return std::find(v.begin(), v.end(), needle) != v.end();
}

// RAII wrapper around PR_SET_NAME. The comm shows up verbatim in the first line
// of /proc/<pid>/status and in /proc/<pid>/stat, so changing it between two
// reads is a deterministic way to make a record grow without depending on
// timing.
class CommGuard {
public:
    explicit CommGuard(const char* name) {
        char buf[16] = {0};
        if (prctl(PR_GET_NAME, buf, 0, 0, 0) == 0) {
            saved_ = buf;
        }
        prctl(PR_SET_NAME, name, 0, 0, 0);
    }
    ~CommGuard() {
        if (!saved_.empty()) {
            prctl(PR_SET_NAME, saved_.c_str(), 0, 0, 0);
        }
    }
    CommGuard(const CommGuard&) = delete;
    CommGuard& operator=(const CommGuard&) = delete;

    static bool Set(const char* name) { return prctl(PR_SET_NAME, name, 0, 0, 0) == 0; }

private:
    std::string saved_;
};

// Long-lived worker thread: publishes its tid, then blocks until the owner
// closes the release pipe. The destructor always joins, so a failing ASSERT
// cannot leave a blocked thread behind for the next case.
class Worker {
public:
    Worker() = default;
    ~Worker() { Stop(); }
    Worker(const Worker&) = delete;
    Worker& operator=(const Worker&) = delete;

    bool Start() {
        if (pipe(ready_) != 0 || pipe(release_) != 0) {
            return false;
        }
        if (pthread_create(&thread_, nullptr, Main, this) != 0) {
            return false;
        }
        started_ = true;
        char byte = 0;
        ssize_t n = 0;
        do {
            n = read(ready_[0], &byte, 1);
        } while (n < 0 && errno == EINTR);
        if (n != 1) {
            Stop();
            return false;
        }
        return true;
    }

    void Stop() {
        if (release_[0] >= 0) {
            close(release_[0]);
            release_[0] = -1;
        }
        if (release_[1] >= 0) {
            close(release_[1]);
            release_[1] = -1;
        }
        if (started_) {
            pthread_join(thread_, nullptr);
            started_ = false;
        }
        if (ready_[0] >= 0) {
            close(ready_[0]);
            ready_[0] = -1;
        }
        if (ready_[1] >= 0) {
            close(ready_[1]);
            ready_[1] = -1;
        }
    }

    long tid() const { return tid_; }

private:
    static void* Main(void* arg) {
        Worker* self = static_cast<Worker*>(arg);
        prctl(PR_SET_NAME, "worker", 0, 0, 0);
        self->tid_ = GetTid();
        const char ready = 'r';
        if (write(self->ready_[1], &ready, 1) != 1) {
            return nullptr;
        }
        char buf[8];
        while (read(self->release_[0], buf, sizeof(buf)) > 0) {
        }
        return nullptr;
    }

    int ready_[2] = {-1, -1};
    int release_[2] = {-1, -1};
    long tid_ = 0;
    pthread_t thread_ = {};
    bool started_ = false;
};


// Reads one line with 1-byte reads, leaving the fd on a line boundary.
bool ReadLine(int fd, std::string* line) {
    line->clear();
    for (;;) {
        char c = 0;
        const ssize_t n = read(fd, &c, 1);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return false;
        }
        if (n == 0) {
            return !line->empty();
        }
        line->push_back(c);
        if (c == '\n') {
            return true;
        }
        if (line->size() > 4096) {
            return false;
        }
    }
}

// True when a mapping line of `maps` covers `addr`. Ranges are compared instead
// of whole lines so that merging with a neighbouring mapping cannot make the
// check miss.
bool MapsCover(const std::string& maps, unsigned long addr) {
    size_t pos = 0;
    while (pos < maps.size()) {
        const size_t nl = maps.find('\n', pos);
        const std::string line =
            (nl == std::string::npos) ? maps.substr(pos) : maps.substr(pos, nl - pos);
        unsigned long start = 0;
        unsigned long end = 0;
        if (sscanf(line.c_str(), "%lx-%lx", &start, &end) == 2 && start <= addr && addr < end) {
            return true;
        }
        if (nl == std::string::npos) {
            break;
        }
        pos = nl + 1;
    }
    return false;
}

// chroot() into a directory that exists or can be created here. Returns false
// when the environment offers none, so the caller can skip instead of failing.
bool ChrootAway() {
    const char* const kCandidates[] = {"/dunitest-chroot", "/tmp", "/dev"};
    for (const char* dir : kCandidates) {
        if (mkdir(dir, 0755) != 0 && errno != EEXIST) {
            continue;
        }
        if (chroot(dir) == 0) {
            return true;
        }
    }
    return false;
}

// Runs in a forked child, so chroot() cannot disturb the suite. Returns 0 when
// the fd kept the view it was opened with, 1 when the environment cannot show
// the difference, and 2 when the fd followed the new root.
int CheckPinnedMountView() {
    UniqueFd fd(open("/proc/self/mountinfo", O_RDONLY));
    if (!fd.valid()) {
        return 1;
    }
    std::string before;
    if (ReadToEof(fd.get(), kReadChunk, &before) != 0 || before.empty()) {
        return 1;
    }

    if (!ChrootAway()) {
        return 1;
    }
    // Evidence that the root really changed: /proc is no longer reachable.
    UniqueFd unreachable(open("/proc/self/mountinfo", O_RDONLY));
    if (unreachable.valid()) {
        return 1;
    }

    // Rewinding forces a re-render: a fd that resolved the target again would
    // render from the new root instead of the one pinned at open.
    if (lseek(fd.get(), 0, SEEK_SET) != 0) {
        return 1;
    }
    std::string after;
    if (ReadToEof(fd.get(), kReadChunk, &after) != 0) {
        return 1;
    }
    return after == before ? 0 : 2;
}

// Reads one byte, retrying on EINTR. Returns -1 with errno set when the read
// fails, 0 at EOF and 1 when a byte was stored.
int ReadByte(int fd, char* out) {
    for (;;) {
        const ssize_t n = read(fd, out, 1);
        if (n < 0 && errno == EINTR) {
            continue;
        }
        return static_cast<int>(n);
    }
}

// Fixed-size payload exchanged with the exec()ed helper over a pipe.
bool WriteRaw(int fd, const void* data, size_t len) {
    const char* p = static_cast<const char*>(data);
    size_t done = 0;
    while (done < len) {
        const ssize_t n = write(fd, p + done, len - done);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return false;
        }
        done += static_cast<size_t>(n);
    }
    return true;
}

bool ReadRaw(int fd, void* data, size_t len) {
    char* p = static_cast<char*>(data);
    size_t done = 0;
    while (done < len) {
        const ssize_t n = read(fd, p + done, len - done);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return false;
        }
        if (n == 0) {
            return false;
        }
        done += static_cast<size_t>(n);
    }
    return true;
}

bool WaitForExit(pid_t pid) {
    for (int i = 0; i < kPollTimeoutMs / 10; ++i) {
        int status = 0;
        if (waitpid(pid, &status, WNOHANG) == pid) {
            return true;
        }
        usleep(10000);
    }
    return false;
}

// Kills and reaps a forked child on scope exit unless it was disarmed. Cases
// that fork a paused child before their last assertion use this instead of a
// trailing kill(), so a failing ASSERT cannot leave that child running (and
// blocking) for the rest of the suite.
class ReapedChild {
public:
    explicit ReapedChild(pid_t pid) : pid_(pid) {}
    ~ReapedChild() { Reap(); }
    ReapedChild(const ReapedChild&) = delete;
    ReapedChild& operator=(const ReapedChild&) = delete;

    void Disarm() { pid_ = -1; }

private:
    void Reap() {
        if (pid_ > 0) {
            kill(pid_, SIGKILL);
            WaitForExit(pid_);
            pid_ = -1;
        }
    }

    pid_t pid_ = -1;
};

}  // namespace

// ---------------------------------------------------------------------------
// 1. one fd == one record
// ---------------------------------------------------------------------------

// Reading to EOF and then reading again must stay at EOF, even when the record
// grew in between. /proc/<pid>/status is a single_open() file, and its Name
// field lets the test grow the record by an exact number of bytes instead of
// hoping that a counter crosses a digit boundary.
TEST(ProcfsTaskSemantics, EofStaysEofWhileContentGrows) {
    CommGuard guard("aaa");

    UniqueFd fd(open("/proc/self/status", O_RDONLY));
    ASSERT_TRUE(fd.valid()) << "cannot open /proc/self/status: errno=" << errno;

    std::string chunked;
    ASSERT_EQ(0, ReadToEof(fd.get(), 13, &chunked)) << "chunked read failed";
    ASSERT_FALSE(chunked.empty());

    ASSERT_TRUE(CommGuard::Set("aaa_procfs_x")) << "cannot grow the record";
    std::string fresh;
    int err = 0;
    ASSERT_TRUE(ReadWholePath("/proc/self/status", &fresh, &err))
        << "cannot re-read /proc/self/status: errno=" << err;
    ASSERT_GT(fresh.size(), chunked.size())
        << "the record did not grow, so this case cannot observe the bug: "
        << Escape(chunked) << " vs " << Escape(fresh);

    char tail[kReadChunk];
    errno = 0;
    const ssize_t extra = read(fd.get(), tail, sizeof(tail));
    ASSERT_GE(extra, 0) << "read after EOF failed: errno=" << errno;
    EXPECT_EQ(0, extra) << "the fd revived EOF and served " << extra
                        << " byte(s) of a longer render: \"" << Escape(std::string(tail, extra))
                        << "\"";
}

// lseek(0) must re-render: the reader sees the current record, not the frozen
// one, matching seq_read_iter()'s ki_pos == 0 reset.
TEST(ProcfsTaskSemantics, RewindRerendersFreshRecord) {
    CommGuard guard("aaa");

    UniqueFd fd(open("/proc/self/status", O_RDONLY));
    ASSERT_TRUE(fd.valid()) << "cannot open /proc/self/status: errno=" << errno;
    std::string first;
    ASSERT_EQ(0, ReadToEof(fd.get(), kReadChunk, &first));

    ASSERT_TRUE(CommGuard::Set("aaa_procfs_x"));
    std::string fresh;
    int err = 0;
    ASSERT_TRUE(ReadWholePath("/proc/self/status", &fresh, &err)) << "errno=" << err;
    ASSERT_GT(fresh.size(), first.size()) << "the record did not grow";

    EXPECT_EQ(std::string::npos, first.find("aaa_procfs_x"));
    EXPECT_NE(std::string::npos, fresh.find("aaa_procfs_x"));

    ASSERT_EQ(0, lseek(fd.get(), 0, SEEK_SET)) << "lseek(0) failed: errno=" << errno;
    std::string reread;
    ASSERT_EQ(0, ReadToEof(fd.get(), kReadChunk, &reread));
    // The current record is identified by its Name field. Exact bytes and byte
    // counts are both unusable here: Time/Stime/vrtime advance between the two
    // reads, and the widths of the counters around them move in either
    // direction, so only the field itself distinguishes a re-render from the
    // frozen snapshot (whose Name is "aaa").
    EXPECT_EQ("aaa_procfs_x", Field(ParseStatus(reread), "Name"))
        << "a rewound fd must serve the current record: " << Escape(reread);
}

// A read position that is neither 0 nor the continuation position re-renders
// the record and serves it from that offset; at or past the end it serves
// nothing. /proc/version never changes, so the expected bytes are exact.
TEST(ProcfsTaskSemantics, SeekIntoRecordServesRecordBytes) {
    const char* kPath = "/proc/version";
    std::string first;
    int err = 0;
    ASSERT_TRUE(ReadWholePath(kPath, &first, &err)) << kPath << ": errno=" << err;
    ASSERT_FALSE(first.empty());

    UniqueFd fd(open(kPath, O_RDONLY));
    ASSERT_TRUE(fd.valid()) << "cannot open " << kPath << ": errno=" << errno;
    std::string full;
    ASSERT_EQ(0, ReadToEof(fd.get(), kReadChunk, &full));
    ASSERT_EQ(first, full);

    const size_t length = first.size();
    const size_t offsets[] = {0, length / 2, length - 1, length, length + 7};
    for (size_t offset : offsets) {
        ASSERT_EQ(static_cast<off_t>(offset), lseek(fd.get(), static_cast<off_t>(offset), SEEK_SET))
            << "lseek(" << offset << ") failed: errno=" << errno;
        std::string got;
        ASSERT_EQ(0, ReadToEof(fd.get(), kReadChunk, &got)) << "offset=" << offset;
        const std::string expected = offset < length ? first.substr(offset) : std::string();
        EXPECT_EQ(expected, got) << "offset=" << offset << " length=" << length;
    }
}

// Guard for the VFS-level behaviour the fix relies on: a procfs lseek(SEEK_END)
// is EINVAL, like seq_lseek().
TEST(ProcfsTaskSemantics, SeekEndIsEinval) {
    UniqueFd fd(open("/proc/self/status", O_RDONLY));
    ASSERT_TRUE(fd.valid()) << "cannot open /proc/self/status: errno=" << errno;
    errno = 0;
    EXPECT_EQ(-1, lseek(fd.get(), 0, SEEK_END));
    EXPECT_EQ(EINVAL, errno);
}

// Every file the fix converted to snapshot reads must survive a chunked read:
// the chunks must stop at EOF, and a second read on the same fd must stay
// there. Byte equality is only required for records that cannot move while the
// test runs; the rest are compared structurally, because their numbers change
// between two reads on Linux as well.
//
// uid_map/gid_map are deliberately absent: reading the *init* user namespace's
// own map deadlocks DragonOS before this change as well (read_at holds
// UserNamespace::inner and generate_content() re-locks the same namespace,
// because an init namespace has no parent to display). That is a separate
// pre-existing bug, so it must not be pinned here.
TEST(ProcfsTaskSemantics, SnapshotSurvivesChunkedRead) {
    const char* const kStrict[] = {
        "/proc/version",
        "/proc/version_signature",
        "/proc/cmdline",
        "/proc/self/cgroup",
        "/proc/self/limits",
    };
    const char* const kStructural[] = {
        "/proc/self/status",
        "/proc/self/stat",
        "/proc/self/statm",
        "/proc/self/maps",
        "/proc/self/mountinfo",
        "/proc/stat",
        "/proc/meminfo",
        "/proc/vmstat",
        "/proc/loadavg",
        "/proc/cpuinfo",
        "/proc/net/arp",
        "/proc/net/protocols",
    };

    for (const char* path : kStrict) {
        UniqueFd fd(open(path, O_RDONLY));
        ASSERT_TRUE(fd.valid()) << "cannot open " << path << ": errno=" << errno;
        std::string chunked;
        ASSERT_EQ(0, ReadToEof(fd.get(), 4, &chunked)) << path;
        char tail[16];
        EXPECT_EQ(0, read(fd.get(), tail, sizeof(tail))) << path << ": EOF was revived";

        std::string fresh;
        int err = 0;
        ASSERT_TRUE(ReadWholePath(path, &fresh, &err)) << path << ": errno=" << err;
        EXPECT_EQ(fresh, chunked) << path << ": a frozen fd must serve its own record";
    }

    for (const char* path : kStructural) {
        UniqueFd fd(open(path, O_RDONLY));
        ASSERT_TRUE(fd.valid()) << "cannot open " << path << ": errno=" << errno;
        std::string chunked;
        ASSERT_EQ(0, ReadToEof(fd.get(), 4, &chunked)) << path;
        char tail[16];
        EXPECT_EQ(0, read(fd.get(), tail, sizeof(tail))) << path << ": EOF was revived";

        std::string fresh;
        int err = 0;
        ASSERT_TRUE(ReadWholePath(path, &fresh, &err)) << path << ": errno=" << err;
        EXPECT_EQ(Shape(fresh), Shape(chunked))
            << path << ": the chunked read produced a different record shape";
    }
}

// A fd that already rendered its snapshot must keep draining it after the
// target thread group is gone. Linux `seq_read_iter()` re-enters a handler only
// when the buffer is empty or the position moved, so a continuation read never
// resolves the target again; resolving it there would turn a live snapshot into
// `ESRCH` and cut the record short.
TEST(ProcfsTaskSemantics, SnapshotSurvivesTargetDeath) {
    const pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno;
    if (child == 0) {
        for (;;) {
            pause();
        }
    }
    ReapedChild child_guard(child);

    const std::string path = StatusPath(child, "mountinfo");
    UniqueFd partial(open(path.c_str(), O_RDONLY));
    ASSERT_TRUE(partial.valid()) << "cannot open " << path << ": errno=" << errno;

    // Take one byte first, so the snapshot exists, and only then read the same
    // record through a second fd while the target is still alive. That gives the
    // expected bytes instead of a byte count that a format change would hide.
    char first = 0;
    ssize_t taken = 0;
    do {
        taken = read(partial.get(), &first, 1);
    } while (taken < 0 && errno == EINTR);
    ASSERT_EQ(1, taken) << "first chunk failed: errno=" << errno;

    std::string whole;
    int err = 0;
    ASSERT_TRUE(ReadWholePath(path, &whole, &err)) << "cannot read " << path << ": errno=" << err;
    ASSERT_GT(whole.size(), 1u) << path << " produced no record";

    kill(child, SIGKILL);
    ASSERT_TRUE(WaitForExit(child)) << "the target was not reaped";
    child_guard.Disarm();

    // Incidental check that the target really is gone for new readers: the fd
    // above is the only thing that may still serve the record. The tid cannot
    // be recycled into a live task within this test's lifetime.
    UniqueFd after(open(path.c_str(), O_RDONLY));
    EXPECT_FALSE(after.valid()) << "a new open still resolved the reaped task";

    // ... but this fd still owns its snapshot and drains it byte for byte.
    std::string rest;
    err = ReadToEof(partial.get(), kReadChunk, &rest);
    EXPECT_EQ(0, err) << "continuation read failed: errno=" << err;
    std::string reassembled;
    reassembled.push_back(first);
    reassembled += rest;
    EXPECT_EQ(whole, reassembled) << "the frozen snapshot was not drained byte for byte";
}

// Reverse guardrail: /proc/<pid>/oom_score_adj is not a seq_file in Linux
// (snprintf + simple_read_from_buffer), so it must keep serving the tail of a
// freshly rendered record instead of freezing one. Converting it would be a
// regression, not a fix.
TEST(ProcfsTaskSemantics, OomScoreAdjStaysStream) {
    const std::string path = StatusPath(getpid(), "oom_score_adj");
    std::string original;
    int err = 0;
    if (!ReadWholePath(path, &original, &err)) {
        GTEST_SKIP() << "cannot read " << path << ": errno=" << err;
    }

    UniqueFd fd(open(path.c_str(), O_RDONLY));
    ASSERT_TRUE(fd.valid()) << "cannot open " << path << ": errno=" << errno;
    std::string first;
    ASSERT_EQ(0, ReadToEof(fd.get(), kReadChunk, &first));
    ASSERT_EQ(original, first);

    const std::string grown = "1000\n";
    UniqueFd wfd(open(path.c_str(), O_WRONLY));
    if (!wfd.valid() || write(wfd.get(), "1000", 4) != 4) {
        GTEST_SKIP() << "cannot grow " << path << ": errno=" << errno
                     << " (this case needs a writable oom_score_adj)";
    }
    if (grown.size() <= first.size()) {
        // The starting value was already long enough; nothing to observe.
        const ssize_t restore = write(wfd.get(), original.c_str(), original.size());
        (void)restore;
        GTEST_SKIP() << "oom_score_adj is already " << Escape(original);
    }

    char tail[32];
    errno = 0;
    const ssize_t n = read(fd.get(), tail, sizeof(tail));
    ASSERT_GE(n, 0) << "read failed: errno=" << errno;
    EXPECT_EQ(grown.substr(first.size()), std::string(tail, static_cast<size_t>(n)))
        << "a non-seq_file must serve the tail of the new render, not EOF";

    // Best effort restore; a non-root caller may be denied the decrease.
    const ssize_t restored = write(wfd.get(), original.c_str(), original.size());
    (void)restored;
}

// ---------------------------------------------------------------------------
// 2. thread-level re-parenting
// ---------------------------------------------------------------------------

struct ReparentReport {
    long original_parent;
    long new_parent;
    long leader_ppid;
    long thread_ppid;
    long worker_tid;
};

// Publishes the worker's tid over a pipe and then stays alive long enough for
// the owner to observe the thread group.
void* PublishTidWorker(void* arg) {
    const int wfd = *static_cast<const int*>(arg);
    prctl(PR_SET_NAME, "worker", 0, 0, 0);
    const long tid = GetTid();
    const ssize_t ignored = write(wfd, &tid, sizeof(tid));
    (void)ignored;
    for (int i = 0; i < 400; ++i) {
        usleep(25000);
    }
    return nullptr;
}

// Read a `long` published by PublishTidWorker; -1 when the pipe closed short.
long ReadPublishedTid(int rfd) {
    long tid = 0;
    size_t got = 0;
    while (got < sizeof(tid)) {
        const ssize_t n = read(rfd, reinterpret_cast<char*>(&tid) + got, sizeof(tid) - got);
        if (n <= 0) {
            break;
        }
        got += static_cast<size_t>(n);
    }
    return got == sizeof(tid) ? tid : -1;
}

// When a thread group is re-parented, every thread must report the new parent:
// /proc/<pid>/task/<tid>/status used to keep the dead parent while
// /proc/<pid>/status reported the adopter.
TEST(ProcfsTaskSemantics, ThreadPpidFollowsGroupReparent) {
    int pipefd[2];
    ASSERT_EQ(0, pipe(pipefd)) << "pipe failed: errno=" << errno;

    const pid_t top = fork();
    ASSERT_GE(top, 0) << "fork failed: errno=" << errno;
    if (top == 0) {
        close(pipefd[0]);
        const pid_t mid = fork();
        if (mid == 0) {
            int tidpipe[2];
            if (pipe(tidpipe) != 0) {
                _exit(3);
            }
            pthread_t th;
            if (pthread_create(&th, nullptr, PublishTidWorker, &tidpipe[1]) != 0) {
                _exit(3);
            }
            const long worker_tid = ReadPublishedTid(tidpipe[0]);
            close(tidpipe[0]);
            close(tidpipe[1]);
            if (worker_tid <= 0) {
                _exit(3);
            }

            ReparentReport rep = {};
            rep.worker_tid = worker_tid;
            rep.original_parent = getppid();
            for (int i = 0; i < 2000; ++i) {
                if (getppid() != rep.original_parent) {
                    break;
                }
                usleep(5000);
            }
            rep.new_parent = getppid();

            std::string text;
            int read_err = 0;
            if (ReadWholePath("/proc/self/status", &text, &read_err)) {
                rep.leader_ppid = strtol(Field(ParseStatus(text), "Ppid").c_str(), nullptr, 10);
            } else {
                rep.leader_ppid = -1;
            }
            const std::string thread_path = TaskStatusPath(getpid(), rep.worker_tid);
            read_err = 0;
            if (ReadWholePath(thread_path, &text, &read_err)) {
                rep.thread_ppid = strtol(Field(ParseStatus(text), "Ppid").c_str(), nullptr, 10);
            } else {
                rep.thread_ppid = -read_err;
            }
            const ssize_t ignored = write(pipefd[1], &rep, sizeof(rep));
            (void)ignored;
            _exit(0);
        }
        // The middle process is the one that dies; its exit is what re-parents
        // the worker's whole thread group.
        usleep(400000);
        _exit(0);
    }

    close(pipefd[1]);
    ReparentReport rep = {};
    size_t got = 0;
    while (got < sizeof(rep)) {
        const ssize_t n = read(pipefd[0], reinterpret_cast<char*>(&rep) + got, sizeof(rep) - got);
        if (n <= 0) {
            break;
        }
        got += static_cast<size_t>(n);
    }
    close(pipefd[0]);
    int status = 0;
    waitpid(top, &status, 0);

    ASSERT_EQ(sizeof(rep), got) << "the re-parented thread group did not report";
    ASSERT_GT(rep.original_parent, 0L);
    ASSERT_GT(rep.new_parent, 0L);
    EXPECT_NE(rep.original_parent, rep.new_parent)
        << "the group was not re-parented, so this case observed nothing";
    EXPECT_EQ(rep.new_parent, rep.leader_ppid);
    EXPECT_EQ(rep.leader_ppid, rep.thread_ppid)
        << "thread " << rep.worker_tid << " reports Ppid=" << rep.thread_ppid
        << " while the group leader reports " << rep.leader_ppid;
}

// ---------------------------------------------------------------------------
// 3. /proc/<nr> lookup vs. /proc listing
// ---------------------------------------------------------------------------

// Any live thread can be named by its own tid, and its directory uses the
// thread-group layout (status/stat/statm/limits/cgroup/maps/...).
TEST(ProcfsTaskSemantics, TidDirectoryResolvesForEveryThread) {
    const pid_t pid = getpid();
    Worker worker;
    ASSERT_TRUE(worker.Start());
    ASSERT_GT(worker.tid(), 0L);
    ASSERT_NE(worker.tid(), GetTid());

    const std::string dir = TidPath(worker.tid());
    const std::vector<std::string> entries = ListDir(dir);
    ASSERT_FALSE(entries.empty()) << "cannot list " << dir << ": errno=" << errno;
    for (const char* name : {"status", "stat", "statm", "limits", "cgroup", "maps", "task"}) {
        EXPECT_TRUE(Contains(entries, name))
            << dir << " is missing " << name << " (entries: " << entries.size() << ")";
    }

    std::string text;
    int err = 0;
    ASSERT_TRUE(ReadWholePath(TidPath(worker.tid(), "status"), &text, &err))
        << "cannot read the thread's own directory: errno=" << err;
    const auto fields = ParseStatus(text);
    EXPECT_EQ(std::to_string(worker.tid()), Field(fields, "Pid"));
    EXPECT_EQ(std::to_string(pid), Field(fields, "Tgid"));
    EXPECT_EQ("worker", Field(fields, "Name"));

    const std::vector<std::string> tids = ListDir(TidPath(worker.tid(), "task"));
    EXPECT_TRUE(Contains(tids, std::to_string(worker.tid())));
    EXPECT_TRUE(Contains(tids, std::to_string(pid)));
}

// /proc *lists* group leaders only, even after a non-leader tid has been named
// through /proc/<tid>. Linux lists through next_tgid() (PIDTYPE_TGID); the
// per-entry cache must not leak a directory that lookup created from the PID
// link.
TEST(ProcfsTaskSemantics, ProcRootListsOnlyGroupLeaders) {
    const pid_t pid = getpid();
    Worker worker;
    ASSERT_TRUE(worker.Start());
    ASSERT_GT(worker.tid(), 0L);

    const std::string tid_path = TidPath(worker.tid(), "status");
    UniqueFd fd(open(tid_path.c_str(), O_RDONLY));
    ASSERT_TRUE(fd.valid()) << "cannot open " << tid_path << ": errno=" << errno;
    fd.Reset();

    const std::vector<std::string> entries = ListDir("/proc");
    ASSERT_FALSE(entries.empty()) << "cannot list /proc: errno=" << errno;
    EXPECT_FALSE(Contains(entries, std::to_string(worker.tid())))
        << "/proc leaked the non-leader tid " << worker.tid();
    EXPECT_TRUE(Contains(entries, std::to_string(pid)))
        << "/proc dropped the group leader " << pid;

    // The tid is still reachable through the thread-group's task directory.
    const std::vector<std::string> tids = ListDir(StatusPath(pid, "task"));
    EXPECT_TRUE(Contains(tids, std::to_string(worker.tid())));
}

// A tid from another thread group must not be reachable below /proc/<pid>/task.
TEST(ProcfsTaskSemantics, ForeignTidUnderTaskIsEnoent) {
    const pid_t pid = fork();
    ASSERT_GE(pid, 0) << "fork failed: errno=" << errno;
    if (pid == 0) {
        for (;;) {
            pause();
        }
    }

    const std::string path = TaskStatusPath(getpid(), pid);
    UniqueFd fd(open(path.c_str(), O_RDONLY));
    const int open_errno = errno;
    EXPECT_FALSE(fd.valid()) << path << " resolved to a foreign task";
    EXPECT_EQ(ENOENT, open_errno) << path << ": errno=" << strerror(open_errno);

    kill(pid, SIGKILL);
    EXPECT_TRUE(WaitForExit(pid));
}

// The issue-reported shape: the group leader exited, a worker is still alive.
// The /proc/<pid>/task subtree and /proc/<pid>/status must stay usable, and the
// zombie leader must not be reaped early.
TEST(ProcfsTaskSemantics, TaskSubtreeSurvivesLeaderExit) {
    int pipefd[2];
    ASSERT_EQ(0, pipe(pipefd)) << "pipe failed: errno=" << errno;

    const pid_t mid = fork();
    ASSERT_GE(mid, 0) << "fork failed: errno=" << errno;
    if (mid == 0) {
        close(pipefd[0]);
        int tidpipe[2];
        if (pipe(tidpipe) != 0) {
            _exit(3);
        }
        pthread_t th;
        if (pthread_create(&th, nullptr, PublishTidWorker, &tidpipe[1]) != 0) {
            _exit(3);
        }
        const long tid = ReadPublishedTid(tidpipe[0]);
        close(tidpipe[0]);
        close(tidpipe[1]);
        const ssize_t ignored = write(pipefd[1], &tid, sizeof(tid));
        (void)ignored;
        close(pipefd[1]);
        usleep(100000);
        // Only the group leader must exit, leaving the group alive. This uses
        // exit(2) rather than pthread_exit(): the latter performs a forced
        // unwind, which gtest's catch(...) swallows, so the child would fall
        // through into the parent's code path instead of exiting the thread.
        syscall(SYS_exit, 0);
    }

    close(pipefd[1]);
    long worker_tid = 0;
    size_t got = 0;
    while (got < sizeof(worker_tid)) {
        const ssize_t n = read(pipefd[0], reinterpret_cast<char*>(&worker_tid) + got,
                               sizeof(worker_tid) - got);
        if (n <= 0) {
            break;
        }
        got += static_cast<size_t>(n);
    }
    close(pipefd[0]);
    ASSERT_EQ(sizeof(worker_tid), got) << "the worker tid was not published";
    usleep(300000);

    const std::vector<std::string> tids = ListDir(StatusPath(mid, "task"));
    EXPECT_EQ(2u, tids.size()) << "the task subtree must list the zombie leader and the worker";

    std::string text;
    int err = 0;
    EXPECT_TRUE(ReadWholePath(StatusPath(mid), &text, &err))
        << "the zombie leader's status is unreadable: errno=" << err;
    EXPECT_TRUE(ReadWholePath(TaskStatusPath(mid, worker_tid), &text, &err))
        << "the live worker's status is unreadable: errno=" << err;

    int status = 0;
    EXPECT_EQ(0, waitpid(mid, &status, WNOHANG))
        << "the group leader was reaped while a worker was still alive";

    kill(static_cast<pid_t>(worker_tid), SIGKILL);
    EXPECT_TRUE(WaitForExit(mid));
}


// ---------------------------------------------------------------------------
// Guardrails for the read paths that changed shape in the same fix
// ---------------------------------------------------------------------------

// /proc/<pid>/maps keeps streaming the mappings that exist when the reader asks
// for the next line, the way Linux m_start()/m_next() re-enter the iteration,
// while the fd still holds one line instead of a copy of the whole table: a
// mapping created after the first read shows up in the rest of the stream.
TEST(ProcfsTaskSemantics, MapsStreamsMappingsAddedAfterFirstRead) {
    UniqueFd fd(open("/proc/self/maps", O_RDONLY));
    ASSERT_TRUE(fd.valid()) << "cannot open /proc/self/maps: errno=" << errno;

    std::string first;
    ASSERT_TRUE(ReadLine(fd.get(), &first)) << "cannot read the first mapping";
    std::string second;
    ASSERT_TRUE(ReadLine(fd.get(), &second)) << "cannot read the second mapping";

    const size_t kLen = 1u << 20;
    void* added = mmap(nullptr, kLen, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, added) << "mmap failed: errno=" << errno;
    const unsigned long added_start = reinterpret_cast<unsigned long>(added);

    std::string rest;
    EXPECT_EQ(0, ReadToEof(fd.get(), kReadChunk, &rest)) << "draining /proc/self/maps failed";

    std::string fresh;
    int err = 0;
    EXPECT_TRUE(ReadWholePath("/proc/self/maps", &fresh, &err)) << "errno=" << err;

    EXPECT_TRUE(MapsCover(fresh, added_start)) << "/proc/self/maps lost the mapping the test created";
    EXPECT_TRUE(MapsCover(rest, added_start))
        << "a mapping created after the first read never showed up in the stream: the fd "
           "served a record that was frozen before it existed";

    munmap(added, kLen);
}

// The length of a read must not change the record it returns. /proc/<pid>/maps
// is rendered one slice at a time, so a single read asking for far more than one
// mapping line reassembles the record from several slices; it must still
// describe exactly what a chunked read of the same position describes.
//
// Both buffers live on the stack, so neither read can move the mapping table the
// other one observes.
TEST(ProcfsTaskSemantics, MapsLargeReadReassemblesTheSameRecord) {
    const size_t kMarkerLen = 1u << 20;
    void* marker =
        mmap(nullptr, kMarkerLen, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, marker) << "mmap failed: errno=" << errno;
    const unsigned long marker_start = reinterpret_cast<unsigned long>(marker);

    UniqueFd fd(open("/proc/self/maps", O_RDONLY));
    ASSERT_TRUE(fd.valid()) << "cannot open /proc/self/maps: errno=" << errno;

    // Several slices: this is much larger than one mapping line.
    char whole[16 * 1024];
    ssize_t n = 0;
    do {
        n = pread(fd.get(), whole, sizeof(whole), 0);
    } while (n < 0 && errno == EINTR);
    ASSERT_GT(n, 0) << "large pread failed: errno=" << errno;

    // The same position, read in small chunks.
    char chunked[sizeof(whole)];
    ssize_t m = 0;
    while (m < static_cast<ssize_t>(sizeof(chunked))) {
        const ssize_t got = pread(fd.get(), chunked + m, kReadChunk, static_cast<off_t>(m));
        if (got < 0) {
            if (errno == EINTR) {
                continue;
            }
            FAIL() << "chunked pread failed: errno=" << errno;
        }
        if (got == 0) {
            break;
        }
        m += got;
    }

    ASSERT_EQ(n, m) << "the record ends at a different place depending on the read length";
    EXPECT_EQ(0, memcmp(whole, chunked, static_cast<size_t>(n)))
        << "the record a read returns depends on how much it asked for";
    EXPECT_TRUE(MapsCover(std::string(whole, static_cast<size_t>(n)), marker_start))
        << "the mapping this test created is missing from the record";

    munmap(marker, kMarkerLen);
}

// /proc/<pid>/mountinfo renders the view its open() pinned, the way
// mounts_open_common() takes get_mnt_ns() + get_fs_root(): changing the root
// after opening the fd must not change what that fd reports.
TEST(ProcfsTaskSemantics, MountInfoKeepsTheRootPinnedAtOpen) {
    const pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno;
    if (child == 0) {
        _exit(CheckPinnedMountView());
    }
    ReapedChild child_guard(child);

    int status = 0;
    bool reaped = false;
    for (int i = 0; i < kPollTimeoutMs / 10; ++i) {
        if (waitpid(child, &status, WNOHANG) == child) {
            reaped = true;
            break;
        }
        usleep(10000);
    }
    ASSERT_TRUE(reaped) << "the child did not finish";
    child_guard.Disarm();
    ASSERT_TRUE(WIFEXITED(status)) << "the child did not exit normally";

    const int code = WEXITSTATUS(status);
    if (code == 1) {
        GTEST_SKIP() << "the guest cannot change its root to show the difference";
    }
    EXPECT_EQ(0, code) << "mountinfo followed a root change made after open()";
}

// ---------------------------------------------------------------------------
// Guardrails for the streaming read path added on top of the record snapshot
// ---------------------------------------------------------------------------

// /proc/<pid>/maps streams one mapping per slice, so killing the target between
// two reads leaves the fd holding the tail of the line it was in the middle of.
// Those bytes are already the reader's: Linux `seq_read_iter()` returns `copied`
// and drops `err` once it copied something, so they must come back instead of
// being thrown away with the `-ESRCH` the *next* record fails with (`m_start()`
// cannot resolve the target any more).
TEST(ProcfsTaskSemantics, MapsStreamKeepsCopiedBytesWhenTargetDies) {
    const pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno;
    if (child == 0) {
        for (;;) {
            pause();
        }
    }
    ReapedChild child_guard(child);

    const std::string path = "/proc/" + std::to_string(child) + "/maps";
    UniqueFd stream(open(path.c_str(), O_RDONLY));
    ASSERT_TRUE(stream.valid()) << "cannot open " << path << ": errno=" << errno;

    // One byte stops the fd inside the first mapping, so the rest of that line
    // is still buffered when the target goes away.
    char first = 0;
    ASSERT_EQ(1, ReadByte(stream.get(), &first)) << "first chunk failed: errno=" << errno;

    std::string whole;
    int err = 0;
    ASSERT_TRUE(ReadWholePath(path, &whole, &err)) << "cannot read " << path << ": errno=" << err;
    ASSERT_GT(whole.size(), 1u) << path << " produced no record";
    ASSERT_EQ(first, whole[0]);

    kill(child, SIGKILL);
    ASSERT_TRUE(WaitForExit(child)) << "the target was not reaped";
    child_guard.Disarm();

    char tail[256];
    ssize_t rest = 0;
    do {
        rest = read(stream.get(), tail, sizeof(tail));
    } while (rest < 0 && errno == EINTR);
    ASSERT_GT(rest, 0) << "the bytes the fd already held were dropped: errno=" << errno;
    EXPECT_EQ(whole.substr(1, static_cast<size_t>(rest)),
              std::string(tail, static_cast<size_t>(rest)))
        << "the fd served something other than the record it started";

    // ... and only then does the unfinished record report its failure.
    errno = 0;
    EXPECT_EQ(-1, read(stream.get(), tail, sizeof(tail)))
        << "the stream continued after the target was gone";
    EXPECT_EQ(ESRCH, errno) << "errno=" << errno;
}

// The address a /proc/<pid>/maps fd resumes from only means something inside the
// address space the fd was opened on, so the file pins it the way Linux
// `proc_maps_open()` -> `proc_mem_open()` does. An execve() in the target must
// therefore leave this fd serving the record it started, not continue (or
// restart, or truncate) its stream in the new image.
//
// The helper exec()s this binary again, so the new image has the same mappings
// plus one marker region it reports back; a fresh read of /proc/<pid>/maps
// proves that image is in place.
TEST(ProcfsTaskSemantics, MapsStreamKeepsTheAddressSpaceOpenedOn) {
    int wake_pipe[2] = {-1, -1};
    int report_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(wake_pipe)) << "pipe failed: errno=" << errno;
    ASSERT_EQ(0, pipe(report_pipe)) << "pipe failed: errno=" << errno;

    const pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno;
    if (child == 0) {
        close(wake_pipe[1]);
        close(report_pipe[0]);
        // The helper must keep both ends across execve(), whatever the default
        // close-on-exec state of this kernel is.
        fcntl(wake_pipe[0], F_SETFD, 0);
        fcntl(report_pipe[1], F_SETFD, 0);
        char go = 0;
        while (ReadByte(wake_pipe[0], &go) < 0) {
        }
        char report_fd[16];
        snprintf(report_fd, sizeof(report_fd), "%d", report_pipe[1]);
        char* const argv[] = {const_cast<char*>("/proc/self/exe"),
                              const_cast<char*>(kMapsExecParkArg), report_fd, nullptr};
        char* const envp[] = {nullptr};
        execve("/proc/self/exe", argv, envp);
        _exit(127);
    }

    close(wake_pipe[0]);
    close(report_pipe[1]);
    UniqueFd wake(wake_pipe[1]);
    UniqueFd report(report_pipe[0]);
    ReapedChild child_guard(child);

    const std::string path = "/proc/" + std::to_string(child) + "/maps";
    UniqueFd stream(open(path.c_str(), O_RDONLY));
    ASSERT_TRUE(stream.valid()) << "cannot open " << path << ": errno=" << errno;

    char first = 0;
    ASSERT_EQ(1, ReadByte(stream.get(), &first)) << "first chunk failed: errno=" << errno;

    // The record this fd started on, taken while the target is parked on the
    // pipe (so its mappings cannot move under the test).
    std::string whole;
    int err = 0;
    ASSERT_TRUE(ReadWholePath(path, &whole, &err)) << "cannot read " << path << ": errno=" << err;
    ASSERT_GT(whole.size(), 1u) << path << " produced no record";
    ASSERT_EQ(first, whole[0]);

    // Let the target execve() and report where its new image mapped a marker.
    ASSERT_TRUE(WriteRaw(wake.get(), "x", 1)) << "cannot wake the target: errno=" << errno;
    unsigned long marker = 0;
    ASSERT_TRUE(ReadRaw(report.get(), &marker, sizeof(marker)))
        << "the exec()ed target did not report its marker";

    std::string fresh;
    ASSERT_TRUE(ReadWholePath(path, &fresh, &err)) << "cannot read " << path << ": errno=" << err;
    ASSERT_TRUE(MapsCover(fresh, marker))
        << "the exec()ed image never published its marker, so this case cannot observe anything";
    // The two records have to be tellable apart, or serving the new one would
    // look the same as serving the old one. Comparing whole records, not the
    // marker address on its own: a new mapping can land on an address the old
    // table already covered with a different range.
    ASSERT_NE(whole, fresh)
        << "both records describe the same address space, so this case cannot tell them apart";

    // The fd keeps serving the address space it was opened on, and only that
    // one: it drains the rest of the line it was inside when the target left,
    // and the mappings of that address space are gone with the execve() (Linux
    // `proc_mem_open()` drops the user reference again, "but do not pin its
    // memory"), so `m_start()` reports EOF instead of continuing the stream.
    std::string rest;
    err = ReadToEof(stream.get(), kReadChunk, &rest);
    EXPECT_EQ(0, err) << "continuation read failed: errno=" << err;
    std::string reassembled;
    reassembled.push_back(first);
    reassembled += rest;
    const size_t first_line = whole.find('\n');
    ASSERT_NE(std::string::npos, first_line) << "the record has no line break";
    EXPECT_EQ(whole.substr(0, first_line + 1), reassembled)
        << "the stream moved on past the address space it was opened on";
}

// A position past the end of a record is EOF, not a rewind: reading there
// returns 0 and keeps returning 0, exactly as `seq_lseek()` -> `traverse()`
// leaves the fd, and moving back to 0 renders the record again
// (`seq_read_iter()`'s `ki_pos == 0` reset).
TEST(ProcfsTaskSemantics, SeekPastEndStaysEofAndRewindReRenders) {
    UniqueFd fd(open("/proc/version", O_RDONLY));
    ASSERT_TRUE(fd.valid()) << "cannot open /proc/version: errno=" << errno;

    std::string whole;
    ASSERT_EQ(0, ReadToEof(fd.get(), kReadChunk, &whole)) << "chunked read failed";
    ASSERT_FALSE(whole.empty());

    const off_t far = 1 << 20;
    ASSERT_EQ(far, lseek(fd.get(), far, SEEK_SET)) << "lseek failed: errno=" << errno;
    char buf[16];
    EXPECT_EQ(0, read(fd.get(), buf, sizeof(buf))) << "a seek past the end must report EOF";
    EXPECT_EQ(0, read(fd.get(), buf, sizeof(buf))) << "EOF must stay EOF";
    EXPECT_EQ(0, pread(fd.get(), buf, sizeof(buf), far))
        << "pread() at the same position must report EOF as well";

    ASSERT_EQ(0, lseek(fd.get(), 0, SEEK_SET)) << "lseek failed: errno=" << errno;
    std::string again;
    EXPECT_EQ(0, ReadToEof(fd.get(), kReadChunk, &again)) << "rewind failed";
    EXPECT_EQ(whole, again) << "rewinding must render the record again";
}

int main(int argc, char** argv) {
    if (argc >= 3 && strcmp(argv[1], kMapsExecParkArg) == 0) {
        const int report_fd = atoi(argv[2]);
        const size_t kMarkerLen = 4u << 20;
        void* marker =
            mmap(nullptr, kMarkerLen, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        const unsigned long addr =
            (marker == MAP_FAILED) ? 0UL : reinterpret_cast<unsigned long>(marker);
        if (addr != 0) {
            WriteRaw(report_fd, &addr, sizeof(addr));
        }
        close(report_fd);
        for (;;) {
            pause();
        }
    }
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
