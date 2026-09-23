// procfs task semantics (issue #2283).
//
// The behaviours pinned here follow the Linux 6.6 model, one section per
// thread of the analysis:
//
//   1. one fd sees one record. Linux serves these files through
//      single_open()/seq_read_iter(), so a read() that reached EOF keeps
//      returning 0 even while the record grows, and moving the file position
//      re-renders. Before the fix every read() re-rendered and the stale byte
//      offset sliced the *new* render, so a second read() could hand back tail
//      bytes of a longer record. The mount tables below stream one mount per
//      slice, so a mount created after an earlier slice is reported by a later
//      one, and opening a fd pins the mount namespace and the root together;
//   2. re-parenting a thread group rewrites the parent links of every thread,
//      so /proc/<pid>/task/<tid>/status and /proc/<pid>/status agree on Ppid;
//   3. /proc/<nr> resolves any task that still holds a PID link (Linux
//      proc_pid_lookup() -> find_task_by_pid_ns()), while /proc *lists* group
//      leaders only (Linux next_tgid()). Everything below such a directory
//      reads the task it names: the fd and fdinfo subtrees walk its files
//      table, mounts/mountinfo render its root, and stat reports whole
//      thread-group accounting even for a hidden tid.
//
// Companion analysis:
//   docs/kernel/filesystem/proc/procfs-task-semantics-root-cause.md
//   docs/kernel/filesystem/proc/procfs-task-semantics-fix-plan.md

#include <gtest/gtest.h>

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
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

/// Channels a probe thread exchanges with its owner: one fixed-size report out,
/// a park until the release pipe reports EOF, and the owner's own argument, the
/// way pthread_create() hands one to a start routine.
struct ProbePayload {
    int report_wfd;
    int release_rfd;
    void* arg;
};

/// Blocks until the owner closes the release pipe.
void ParkUntilReleased(int release_rfd) {
    char buf[8];
    while (read(release_rfd, buf, sizeof(buf)) > 0) {
    }
}

/// Owns a probe thread, the pipes it reports over, and a join that always runs,
/// so a failed assertion cannot leave the thread parked behind the case.
///
/// Prepare() opens the pipes before Launch() starts the body, so a body that
/// needs to name a pipe descriptor can be handed it in its argument. The table
/// is shared with the thread, so a body that gave itself a private copy has to
/// close its own copy of the release write end, or the park below can never see
/// the owner release it.
class ProbeThread {
public:
    using Body = void* (*)(void*);

    ProbeThread() = default;
    ~ProbeThread() { Stop(); }
    ProbeThread(const ProbeThread&) = delete;
    ProbeThread& operator=(const ProbeThread&) = delete;

    bool Prepare() {
        if (pipe(report_) != 0 || pipe(release_) != 0) {
            return false;
        }
        payload_.report_wfd = report_[1];
        payload_.release_rfd = release_[0];
        return true;
    }

    /// The descriptor a body has to leave alone, or close in a private copy of
    /// the table, to make the parked thread release.
    int release_write_fd() const { return release_[1]; }

    bool Launch(Body body, void* arg) {
        payload_.arg = arg;
        if (pthread_create(&thread_, nullptr, body, &payload_) != 0) {
            return false;
        }
        started_ = true;
        return true;
    }

    /// The thread's fixed-size report. The pipes stay open until Stop(), the
    /// way the descriptor table is shared with the thread.
    bool ReadReport(void* out, size_t len) { return ReadRaw(report_[0], out, len); }

    void Stop() {
        if (release_[1] >= 0) {
            close(release_[1]);
            release_[1] = -1;
        }
        if (started_) {
            pthread_join(thread_, nullptr);
            started_ = false;
        }
        for (int i = 0; i < 2; ++i) {
            if (report_[i] >= 0) {
                close(report_[i]);
                report_[i] = -1;
            }
            if (release_[i] >= 0) {
                close(release_[i]);
                release_[i] = -1;
            }
        }
    }

private:
    ProbePayload payload_ = {-1, -1, nullptr};
    int report_[2] = {-1, -1};
    int release_[2] = {-1, -1};
    pthread_t thread_ = {};
    bool started_ = false;
};

/// Names itself "worker", publishes its tid, then parks: the plain probe thread
/// the tid-addressed cases send in.
void* TidWorker(void* arg) {
    ProbePayload* payload = static_cast<ProbePayload*>(arg);
    prctl(PR_SET_NAME, "worker", 0, 0, 0);
    const long tid = GetTid();
    WriteRaw(payload->report_wfd, &tid, sizeof(tid));
    ParkUntilReleased(payload->release_rfd);
    return nullptr;
}

/// Long-lived worker thread: publishes its tid, then blocks until the owner
/// closes the release pipe. The destructor always joins, so a failing ASSERT
/// cannot leave a blocked thread behind for the next case.
class Worker {
public:
    Worker() = default;
    ~Worker() { Stop(); }
    Worker(const Worker&) = delete;
    Worker& operator=(const Worker&) = delete;

    bool Start() {
        if (!probe_.Prepare() || !probe_.Launch(TidWorker, nullptr) ||
            !probe_.ReadReport(&tid_, sizeof(tid_))) {
            Stop();
            return false;
        }
        return true;
    }

    void Stop() { probe_.Stop(); }

    long tid() const { return tid_; }

private:
    ProbeThread probe_;
    long tid_ = 0;
};

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
// uid_map/gid_map are covered by user_namespace_id_map.cc rather than repeated
// in this generic procfs snapshot matrix.
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

struct GroupReparentReport {
    long original_parent;
    long new_parent;
    long leader_ppid;
    long thread_ppid;
    long worker_tid;
};

// When a thread group is re-parented, every thread must report the new parent:
// /proc/<pid>/task/<tid>/status used to keep the dead parent while
// /proc/<pid>/status reported the adopter.
//
// The parent is not put to sleep and hoped to still be there: it waits on a
// pipe until the group has read the Ppid that names it and has its second
// thread parked, so the observation cannot lose the race against the exit that
// triggers the re-parenting.
TEST(ProcfsTaskSemantics, ThreadPpidFollowsGroupReparent) {
    int ready[2];
    int report[2];
    ASSERT_EQ(0, pipe(ready)) << "pipe failed: errno=" << errno;
    ASSERT_EQ(0, pipe(report)) << "pipe failed: errno=" << errno;

    const pid_t dying = fork();
    ASSERT_GE(dying, 0) << "fork failed: errno=" << errno;
    if (dying == 0) {
        // This process only exists to die: its exit is what re-parents the
        // group below. The pipes are duplicated into that group, so the
        // descriptors this process does not need are closed only after the
        // fork that created it.
        close(report[0]);
        const pid_t group = fork();
        if (group == 0) {
            close(ready[0]);
            GroupReparentReport rep = {};
            Worker worker;
            if (!worker.Start()) {
                _exit(3);
            }
            rep.worker_tid = worker.tid();
            rep.original_parent = getppid();

            // Let the parent go only now that `original_parent` is sampled, and
            // keep the second thread parked until both views were read back, so
            // neither number can be lost to a dead target.
            const char go = 'g';
            if (write(ready[1], &go, 1) != 1) {
                _exit(3);
            }
            close(ready[1]);

            for (int i = 0; i < 2000 && getppid() == rep.original_parent; ++i) {
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
            read_err = 0;
            if (ReadWholePath(TaskStatusPath(getpid(), rep.worker_tid), &text, &read_err)) {
                rep.thread_ppid = strtol(Field(ParseStatus(text), "Ppid").c_str(), nullptr, 10);
            } else {
                rep.thread_ppid = -read_err;
            }

            const bool reported = WriteRaw(report[1], &rep, sizeof(rep));
            worker.Stop();
            _exit(reported ? 0 : 3);
        }
        close(ready[1]);
        close(report[1]);
        char token = 0;
        ssize_t n = 0;
        do {
            n = read(ready[0], &token, 1);
        } while (n < 0 && errno == EINTR);
        close(ready[0]);
        _exit(0);
    }

    close(ready[0]);
    close(ready[1]);
    close(report[1]);
    GroupReparentReport rep = {};
    const bool got = ReadRaw(report[0], &rep, sizeof(rep));
    close(report[0]);
    int status = 0;
    waitpid(dying, &status, 0);

    ASSERT_TRUE(got) << "the re-parented thread group did not report";
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
        Worker worker;
        if (!worker.Start()) {
            _exit(3);
        }
        const long tid = worker.tid();
        const bool reported = WriteRaw(pipefd[1], &tid, sizeof(tid));
        close(pipefd[1]);
        if (!reported) {
            _exit(3);
        }
        usleep(100000);
        // Only the group leader must exit, leaving the group alive. This uses
        // exit(2) rather than pthread_exit(): the latter performs a forced
        // unwind, which gtest's catch(...) swallows, so the child would fall
        // through into the parent's code path instead of exiting the thread.
        syscall(SYS_exit, 0);
    }

    close(pipefd[1]);
    long worker_tid = 0;
    const bool published = ReadRaw(pipefd[0], &worker_tid, sizeof(worker_tid));
    close(pipefd[0]);
    ASSERT_TRUE(published) << "the worker tid was not published";

    // Wait for the leader to actually be gone instead of assuming it after a
    // fixed delay: every assertion below is only about the shape the group has
    // once its leader exited.
    bool leader_exited = false;
    for (int i = 0; i < kPollTimeoutMs / 10 && !leader_exited; ++i) {
        std::string text;
        int err = 0;
        if (ReadWholePath(StatusPath(mid), &text, &err)) {
            leader_exited = Field(ParseStatus(text), "State").find("Exited") != std::string::npos;
        }
        if (!leader_exited) {
            usleep(10000);
        }
    }
    ASSERT_TRUE(leader_exited) << "the group leader never exited";

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

// ---------------------------------------------------------------------------
// 4. per-thread state behind a hidden tid
// ---------------------------------------------------------------------------
//
// Linux builds /proc/<nr> from the task proc_pid_lookup() found, so everything
// below it reads *that* task: proc_fd_link()/proc_readfd_common() walk its
// files table, mounts_open_common() pins its mount namespace and root, and
// proc_tgid_stat() still reports whole-thread-group accounting. The cases below
// pin that against a thread which took private state, and against a hidden tid
// whose stat record used to mix the per-thread and thread-group views.

#ifndef SYS_close_range
#define SYS_close_range 436
#endif
#ifndef SYS_unshare
#define SYS_unshare 272
#endif

/// close_range(2) flag that gives the calling thread its own files table.
constexpr unsigned kCloseRangeUnshare = 1u << 1;
/// unshare(2) flag that gives the calling thread its own fs_struct.
constexpr unsigned kCloneFs = 0x00000200;
namespace {

/// Reports a thread's tid plus what its private-table setup did.
struct PrivateTableReport {
    long tid;
    int unshare_errno;
};

/// What the private-table probe has to name: the descriptor it punches out of
/// its own copy of the table, and the release write end it has to close in that
/// copy before parking.
struct PrivateTableArg {
    int punch_fd;
    int release_wfd;
};

/// Tid of the thread that burned CPU, and the burn's own result.
struct BusyReport {
    long tid;
};

/// Fields of a /proc/<pid>/stat line, indexed from 1 (field 1 is the pid).
/// Linux keeps the command in field 2 inside parentheses, so the split happens
/// after the closing one.
std::vector<std::string> ParseStatFields(const std::string& text) {
    std::vector<std::string> fields;
    const size_t open = text.find('(');
    const size_t close = text.rfind(')');
    if (open == std::string::npos || close == std::string::npos || close < open) {
        return fields;
    }
    fields.push_back(Trim(text.substr(0, open)));
    fields.push_back(text.substr(open + 1, close - open - 1));
    size_t pos = close + 1;
    while (pos < text.size()) {
        while (pos < text.size() && (text[pos] == ' ' || text[pos] == '\n' || text[pos] == '\0')) {
            ++pos;
        }
        size_t end = pos;
        while (end < text.size() && text[end] != ' ' && text[end] != '\n' && text[end] != '\0') {
            ++end;
        }
        if (end > pos) {
            fields.push_back(text.substr(pos, end - pos));
        }
        pos = end;
    }
    return fields;
}

/// One numeric field of a /proc/<pid>/stat line, or -1 when the file cannot be
/// read or the field is missing.
long StatFieldOf(const std::string& path, size_t index) {
    std::string text;
    int err = 0;
    if (!ReadWholePath(path, &text, &err)) {
        return -1;
    }
    const std::vector<std::string> fields = ParseStatFields(text);
    if (index == 0 || index > fields.size()) {
        return -1;
    }
    return strtol(fields[index - 1].c_str(), nullptr, 10);
}

/// Gives the calling thread a files table of its own with one descriptor
/// punched out of it. Only close_range(CLOSE_RANGE_UNSHARE) hands a single
/// thread a private table, so this is the setup the hidden-tid fd paths have to
/// follow.
void* PrivateTableWorker(void* arg) {
    ProbePayload* payload = static_cast<ProbePayload*>(arg);
    const PrivateTableArg* setup = static_cast<const PrivateTableArg*>(payload->arg);
    PrivateTableReport report = {};
    report.tid = GetTid();
    const long unshared = syscall(SYS_close_range, setup->punch_fd, setup->punch_fd,
                                  kCloseRangeUnshare);
    report.unshare_errno = (unshared == 0) ? 0 : errno;
    // This thread's table is private now, so it holds its own copy of the
    // release write end: leaving it open would keep the park below from seeing
    // the owner release the pipe.
    close(setup->release_wfd);
    WriteRaw(payload->report_wfd, &report, sizeof(report));
    ParkUntilReleased(payload->release_rfd);
    return nullptr;
}

/// Whether `/proc/<tid>/stat` and `/proc/<pid>/stat` report the same CPU time.
struct PrivateRootReport {
    long tid;
    int unshare_errno;
    int chroot_errno;
};

/// Gives the calling thread its own fs_struct, changes its root, then parks.
void* PrivateRootWorker(void* arg) {
    ProbePayload* payload = static_cast<ProbePayload*>(arg);
    PrivateRootReport report = {};
    report.tid = GetTid();
    const long unshared = syscall(SYS_unshare, kCloneFs);
    report.unshare_errno = (unshared == 0) ? 0 : errno;
    if (unshared == 0) {
        report.chroot_errno = ChrootAway() ? 0 : ENOENT;
    }
    WriteRaw(payload->report_wfd, &report, sizeof(report));
    ParkUntilReleased(payload->release_rfd);
    return nullptr;
}

long long MonotonicMs() {
    struct timespec ts = {};
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return static_cast<long long>(ts.tv_sec) * 1000 + ts.tv_nsec / 1000000;
}

/// Burns user time in this thread, which is not the group leader, then parks.
void* BusyWorker(void* arg) {
    ProbePayload* payload = static_cast<ProbePayload*>(arg);
    BusyReport report = {};
    report.tid = GetTid();
    const long long started = MonotonicMs();
    volatile unsigned long long sink = 1;
    while (MonotonicMs() - started < 300) {
        for (int i = 0; i < 100000; ++i) {
            sink = sink * 6364136223846793005ULL + 1442695040888963407ULL;
        }
    }
    WriteRaw(payload->report_wfd, &report, sizeof(report));
    ParkUntilReleased(payload->release_rfd);
    return nullptr;
}

}  // namespace

// A thread that took its own files table with close_range(CLOSE_RANGE_UNSHARE)
// can hold a descriptor set the group leader does not. Linux resolves
// /proc/<tid>/fd and fdinfo through get_proc_task(inode), so the thread's own
// directory reports what it closed and the leader's still reports the
// descriptor it kept.
TEST(ProcfsTaskSemantics, TidFdSubtreeUsesTheThreadsFilesTable) {
    const pid_t pid = getpid();
    // The descriptor the thread punches out of its own copy. The leader keeps
    // it, which is what makes the two views tell each other.
    UniqueFd punch(open("/proc/version", O_RDONLY));
    ASSERT_TRUE(punch.valid()) << "cannot open the descriptor to punch: errno=" << errno;

    ProbeThread probe;
    ASSERT_TRUE(probe.Prepare()) << "cannot create the probe pipes: errno=" << errno;
    PrivateTableArg setup = {punch.get(), probe.release_write_fd()};
    ASSERT_TRUE(probe.Launch(PrivateTableWorker, &setup))
        << "pthread_create failed: errno=" << errno;

    PrivateTableReport report = {};
    ASSERT_TRUE(probe.ReadReport(&report, sizeof(report)))
        << "the probe thread did not report: errno=" << errno;
    ASSERT_EQ(0, report.unshare_errno)
        << "close_range(CLOSE_RANGE_UNSHARE) failed: errno=" << report.unshare_errno;

    const std::string fd_name = std::to_string(punch.get());
    const std::string group_fd_dir = StatusPath(pid, "fd");
    const std::string thread_fd_dir = TidPath(report.tid, "fd");

    // The leader still holds the descriptor, so the thread really took a copy
    // of the table instead of closing the group's descriptor.
    EXPECT_TRUE(Contains(ListDir(group_fd_dir), fd_name))
        << group_fd_dir << " lost fd " << fd_name << ": the thread closed the group's table";

    // The thread's directory is the one that must not list it.
    EXPECT_FALSE(Contains(ListDir(thread_fd_dir), fd_name))
        << thread_fd_dir << " lists fd " << fd_name << ", which the thread removed from its table";

    char link_target[256] = {0};
    const std::string link_path = TidPath(report.tid, ("fd/" + fd_name).c_str());
    const ssize_t link_len = readlink(link_path.c_str(), link_target, sizeof(link_target) - 1);
    const int link_errno = errno;
    EXPECT_LT(link_len, 0) << link_path << " resolved fd " << fd_name << " of the group's table to "
                           << std::string(link_target, sizeof(link_target));
    EXPECT_EQ(ENOENT, link_errno) << link_path << ": errno=" << strerror(link_errno);

    // fdinfo resolves through the same table: the entry is gone for the thread
    // and still there for the leader.
    UniqueFd thread_fdinfo(
        open(TidPath(report.tid, ("fdinfo/" + fd_name).c_str()).c_str(), O_RDONLY));
    EXPECT_FALSE(thread_fdinfo.valid())
        << "the thread's fdinfo resolved fd " << fd_name << ", which it removed from its table";
    UniqueFd group_fdinfo(open(StatusPath(pid, ("fdinfo/" + fd_name).c_str()).c_str(), O_RDONLY));
    EXPECT_TRUE(group_fdinfo.valid())
        << "the leader lost its own fdinfo entry: errno=" << errno;
}

// Observes /proc/<tid>/mounts from a thread with its own root. Runs in a forked
// child: if the guest refused to unshare the fs_struct, chroot() would move the
// whole suite's root, which must not be allowed to happen in the test process.
int CheckTidMountView() {
    ProbeThread probe;
    if (!probe.Prepare() || !probe.Launch(PrivateRootWorker, nullptr)) {
        return 1;
    }

    PrivateRootReport report = {};
    const bool reported = probe.ReadReport(&report, sizeof(report));
    int result = 1;
    if (reported && report.unshare_errno == 0 && report.chroot_errno == 0) {
        const std::string leader_path = "/proc/" + std::to_string(getpid()) + "/mounts";
        const std::string tid_path = "/proc/" + std::to_string(report.tid) + "/mounts";
        std::string leader;
        std::string thread_view;
        int err = 0;
        if (ReadWholePath(leader_path, &leader, &err) && ReadWholePath(tid_path, &thread_view, &err)) {
            // A non-empty leader record keeps the comparison from being
            // vacuous; only the thread changed its root, so the two must differ.
            result = (leader.empty() || thread_view == leader) ? 2 : 0;
        }
    }
    // The probe owns the pipes and the join, so returning releases the parked
    // thread even when an early branch gave up on the comparison.
    return result;
}

// /proc/<tid>/mounts renders from the task the proc inode names: a thread that
// unshared its fs_struct and changed root must not report the group leader's
// view. Linux mounts_open_common() pins get_proc_task(inode), not the leader.
TEST(ProcfsTaskSemantics, TidMountsUsesTheThreadsRoot) {
    const pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno;
    if (child == 0) {
        _exit(CheckTidMountView());
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
        GTEST_SKIP() << "the guest cannot give a thread its own root";
    }
    EXPECT_EQ(0, code) << "a thread's /proc/<tid>/mounts reported the group leader's view";
}

// `/proc/<nr>/stat` is proc_tgid_stat() even when `nr` names a non-leader
// thread, so its CPU time (14/15) aggregates the thread group the same way its
// fault counters (10/12) do. Reading one task group through two directories
// must not produce two different totals.
TEST(ProcfsTaskSemantics, TidStatReportsThreadGroupUsage) {
    const pid_t pid = getpid();
    ProbeThread probe;
    ASSERT_TRUE(probe.Prepare()) << "cannot create the probe pipes: errno=" << errno;
    ASSERT_TRUE(probe.Launch(BusyWorker, nullptr)) << "pthread_create failed: errno=" << errno;

    BusyReport report = {};
    ASSERT_TRUE(probe.ReadReport(&report, sizeof(report)))
        << "the busy thread did not report: errno=" << errno;
    ASSERT_GT(report.tid, 0L);
    ASSERT_NE(report.tid, GetTid()) << "the probe is not a separate thread";

    // The group leader has run the whole suite while the probe thread burned
    // 300ms of user time, so the per-thread and thread-group totals differ by
    // far more than the tick granularity of the two reads.
    const long group_utime = StatFieldOf(StatusPath(pid, "stat"), 14);
    const long tid_utime = StatFieldOf(TidPath(report.tid, "stat"), 14);
    const long thread_utime = StatFieldOf(TaskStatusPath(pid, report.tid, "stat"), 14);
    ASSERT_GE(group_utime, 0L) << "cannot read /proc/<pid>/stat";
    ASSERT_GE(tid_utime, 0L) << "cannot read /proc/<tid>/stat";
    ASSERT_GE(thread_utime, 0L) << "cannot read /proc/<pid>/task/<tid>/stat";

    EXPECT_NEAR(static_cast<double>(group_utime), static_cast<double>(tid_utime), 3.0)
        << "the hidden tid path reported utime=" << tid_utime
        << " while the group leader reported " << group_utime
        << ": the record mixed per-thread CPU time with thread-group accounting";
    EXPECT_GT(thread_utime, 0L) << "the thread that burned CPU reported none";
    EXPECT_LE(thread_utime, tid_utime + 3)
        << "the per-thread view " << thread_utime << " is above the group total " << tid_utime;
}

// /proc/net/arp is served one entry per slice (Linux arp_seq_ops), so a reader
// that takes the record one byte at a time must reassemble it exactly, header
// included. A renderer that treated every slice as the first would repeat the
// header; one that dropped its cursor would lose entries.
//
// A guest without a network device renders the header alone, so what this pins
// there is the slicing contract (one header, byte-exact reassembly) rather than
// a populated table.
TEST(ProcfsTaskSemantics, ArpChunkedReadReassemblesTheSameRecord) {
    std::string whole;
    int err = 0;
    ASSERT_TRUE(ReadWholePath("/proc/net/arp", &whole, &err))
        << "cannot read /proc/net/arp: errno=" << err;
    ASSERT_FALSE(whole.empty()) << "/proc/net/arp produced no record";

    UniqueFd fd(open("/proc/net/arp", O_RDONLY));
    ASSERT_TRUE(fd.valid()) << "cannot open /proc/net/arp: errno=" << errno;
    std::string chunked;
    ASSERT_EQ(0, ReadToEof(fd.get(), 1, &chunked)) << "single-byte read failed: errno=" << errno;
    EXPECT_EQ(whole, chunked) << "the single-byte read did not reassemble the record";

    const std::string header =
        "IP address       HW type     Flags       HW address            Mask     Device\n";
    ASSERT_GE(chunked.size(), header.size()) << "the record is shorter than its header";
    EXPECT_EQ(header, chunked.substr(0, header.size()));
    EXPECT_EQ(std::string::npos, chunked.find(header, header.size()))
        << "the header was rendered again on a later slice";
}

// ---------------------------------------------------------------------------
// Guardrails for the streaming mount table
// ---------------------------------------------------------------------------

namespace {

/// `unshare(2)` flag that asks for a mount namespace of its own.
constexpr unsigned kCloneNewMountNs = 0x00020000;

/// The first two fields of a mountinfo record: the id of the mount and the id
/// of the mount it is attached below.
struct MountIdPair {
    unsigned long id;
    unsigned long parent;
};

/// The id pair of the first record of `record`.
bool FirstMountIdPair(const std::string& record, MountIdPair* out) {
    const size_t end = record.find('\n');
    const std::string line = (end == std::string::npos) ? record : record.substr(0, end);
    return sscanf(line.c_str(), "%lu %lu", &out->id, &out->parent) == 2;
}

/// Reads one byte of `path`, mounts a tmpfs on `dir` while that fd stays open,
/// then drains the fd. Returns 0 when the rest of the stream reports the mount
/// point, 2 when it does not, 3 when the table produced no first record, and 1
/// when the guest cannot run the setup.
int CheckMountCreatedAfterFirstReadIsStreamed(const char* path, const std::string& dir) {
    UniqueFd fd(open(path, O_RDONLY));
    if (!fd.valid()) {
        return 1;
    }
    char first = 0;
    if (ReadByte(fd.get(), &first) != 1) {
        // Every namespace has a root mount, so a table that hands out no record
        // at its first byte is the regression this case exists for, not an
        // environment the guest cannot provide.
        return 3;
    }

    if (mkdir(dir.c_str(), 0755) != 0 && errno != EEXIST) {
        return 1;
    }
    if (mount("none", dir.c_str(), "tmpfs", 0, nullptr) != 0) {
        return 1;
    }

    std::string rest(1, first);
    const int read_errno = ReadToEof(fd.get(), kReadChunk, &rest);
    const bool streamed = rest.find(dir) != std::string::npos;
    umount(dir.c_str());
    rmdir(dir.c_str());
    return (read_errno == 0 && streamed) ? 0 : 2;
}

/// Runs the check for both names the mount table is reachable under. Runs in a
/// forked child that took a mount namespace of its own, so the tmpfs the case
/// mounts cannot leak into the rest of the suite.
int CheckMountStreamChild() {
    if (syscall(SYS_unshare, kCloneNewMountNs) != 0) {
        return 1;
    }
    const int mountinfo = CheckMountCreatedAfterFirstReadIsStreamed(
        "/proc/self/mountinfo", "/tmp/dunitest_mount_stream_info");
    if (mountinfo != 0) {
        return mountinfo;
    }
    return CheckMountCreatedAfterFirstReadIsStreamed("/proc/self/mounts",
                                                     "/tmp/dunitest_mount_stream");
}

/// Announces `step` to the owner and waits for it to answer.
bool AnnounceAndWait(int report_wfd, int go_rfd, int step) {
    return WriteRaw(report_wfd, &step, sizeof(step)) && ReadRaw(go_rfd, &step, sizeof(step));
}

/// How long the child below keeps taking mount namespaces before it stops on
/// its own. It is longer than the reader's window on purpose: the reader stops
/// the child by closing the pipe, and this bound only keeps a child whose owner
/// is already gone from unsharing forever.
constexpr long long kNamespaceLoopChildMs = 6000;

/// Takes a mount namespace of its own, announces it and waits for the owner,
/// then keeps replacing its mount namespace until the owner closes the pipe or
/// the child reaches its own deadline. Returns 0 when it stopped, and the errno
/// of the step that failed otherwise. Runs in a forked child: the owner needs a
/// task that keeps moving through mount namespaces, each with the root that came
/// with it.
int CheckNamespaceLoopChild(int report_wfd, int go_rfd) {
    const long long deadline = MonotonicMs() + kNamespaceLoopChildMs;
    bool announced = false;
    while (MonotonicMs() < deadline) {
        if (syscall(SYS_unshare, kCloneNewMountNs) != 0) {
            const int failed = errno;
            return failed != 0 ? failed : 1;
        }
        if (!announced) {
            announced = true;
            if (!AnnounceAndWait(report_wfd, go_rfd, 1)) {
                return 1;
            }
        }
        struct pollfd release = {go_rfd, POLLIN, 0};
        if (poll(&release, 1, 0) > 0) {
            return 0;
        }
    }
    return 0;
}

}  // namespace

// A mount created after an earlier slice of /proc/<pid>/mountinfo belongs to a
// later slice: Linux renders one record per show() call, so the iteration sees
// the topology the namespace has when the reader asks for the next record. An
// fd that rendered the table once, on its first read, keeps serving that render
// and never reports the new mount point.
TEST(ProcfsTaskSemantics, MountTableStreamsMountsCreatedAfterFirstRead) {
    const pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno;
    if (child == 0) {
        _exit(CheckMountStreamChild());
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
        GTEST_SKIP() << "the guest cannot open a mount table or mount a tmpfs in a private "
                        "mount namespace";
    }
    ASSERT_NE(3, code) << "the first read of the mount table produced no record";
    EXPECT_EQ(0, code)
        << "the rest of the stream did not report a mount created after the first read";
}

// The read length must not decide what the fd reports: /proc/<pid>/mountinfo
// hands out one slice of records per read, so a read that takes the record one
// byte at a time reassembles it exactly. A renderer that treated every slice as
// the first would repeat the leading records, and one that dropped its cursor
// would lose records. The two reads of a path are taken back to back and nothing
// else in this suite mounts in the namespace the case itself runs in (the cases
// that mount do so in a child of their own with a mount namespace of its own),
// so the record cannot move between them and the comparison can be exact.
TEST(ProcfsTaskSemantics, MountTableChunkedReadReassemblesTheSameRecord) {
    const std::pair<const char*, bool> kPaths[] = {
        {"/proc/self/mountinfo", true},
        {"/proc/self/mounts", true},
        {"/proc/self/mountstats", false},
    };
    for (const auto& [path, non_empty] : kPaths) {
        std::string whole;
        int err = 0;
        ASSERT_TRUE(ReadWholePath(path, &whole, &err)) << path << ": errno=" << err;
        if (non_empty) {
            ASSERT_FALSE(whole.empty()) << path << " produced no record";
        }

        UniqueFd fd(open(path, O_RDONLY));
        ASSERT_TRUE(fd.valid()) << path << ": errno=" << errno;
        std::string chunked;
        ASSERT_EQ(0, ReadToEof(fd.get(), 1, &chunked)) << path << ": single-byte read failed";
        EXPECT_EQ(whole, chunked) << path << ": the record a read returns depends on its length";
    }
}

// Opening the file pins the mount namespace and the root that the record is
// rendered from, and a mount namespace switch publishes the two as one unit, so
// a reader of /proc/<pid>/mountinfo can only ever see a whole generation. The
// child below keeps replacing its mount namespace while the owner reads, and
// every record must look like a whole generation: a record whose first mount is
// its own parent is the signature of a capture that took the mount namespace of
// one generation and the root of the next (a namespace root is rendered with
// the id of its invisible parent). A guest that reports a namespace root as its
// own parent cannot show the difference and is skipped.
TEST(ProcfsTaskSemantics, MountViewIsOneNamespaceGeneration) {
    int report[2] = {-1, -1};
    int go[2] = {-1, -1};
    ASSERT_EQ(0, pipe(report)) << "pipe failed: errno=" << errno;
    ASSERT_EQ(0, pipe(go)) << "pipe failed: errno=" << errno;

    const pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno;
    if (child == 0) {
        close(report[0]);
        close(go[1]);
        _exit(CheckNamespaceLoopChild(report[1], go[0]));
    }
    ReapedChild child_guard(child);
    close(report[1]);
    close(go[0]);

    const std::string path = "/proc/" + std::to_string(child) + "/mountinfo";
    int step = 0;
    if (!ReadRaw(report[0], &step, sizeof(step))) {
        close(report[0]);
        close(go[1]);
        GTEST_SKIP() << "the child could not unshare a mount namespace";
    }
    ASSERT_EQ(1, step);
    std::string quiet;
    int err = 0;
    if (!ReadWholePath(path, &quiet, &err)) {
        close(report[0]);
        close(go[1]);
        GTEST_SKIP() << "cannot read " << path << ": errno=" << err;
    }
    MountIdPair quiet_root = {};
    if (!FirstMountIdPair(quiet, &quiet_root)) {
        close(report[0]);
        close(go[1]);
        GTEST_SKIP() << "this guest renders no mountinfo record";
    }
    if (quiet_root.id == quiet_root.parent) {
        close(report[0]);
        close(go[1]);
        GTEST_SKIP() << "this guest reports a namespace root with its own id as parent";
    }
    int answer = 1;
    ASSERT_TRUE(WriteRaw(go[1], &answer, sizeof(answer)));

    // An empty record is tolerated on purpose: a kernel that pins the two slots
    // in two steps can leave the reader with the root of one generation and the
    // mount namespace of the other, and the mounts of the pinned namespace are
    // then unreachable from the pinned root, so the record comes back empty
    // (Linux 6.6 does this on a measurable share of such reads). The property
    // this case pins has to hold either way: a record that is not a whole
    // generation is a mixture.
    bool mixed = false;
    std::string mixed_record;
    bool read_failed = false;
    int failed_errno = 0;
    size_t empty_records = 0;
    std::set<unsigned long> generations;
    size_t reads = 0;
    const long long deadline = MonotonicMs() + 1500;
    while (MonotonicMs() < deadline && reads < 20000) {
        std::string record;
        int read_errno = 0;
        const bool readable = ReadWholePath(path, &record, &read_errno);
        ++reads;
        if (!readable) {
            // A generation that cannot be rendered at all is a failure of its
            // own: the two slots are published as one pair, so every read has a
            // whole generation to render and none of them may fail.
            read_failed = true;
            failed_errno = read_errno;
            break;
        }
        MountIdPair root = {};
        if (!FirstMountIdPair(record, &root)) {
            // Every generation has a root mount, so a record with no mount in it
            // is counted rather than ignored: a run that only ever saw those is
            // not the same as one that only ever saw a single generation.
            ++empty_records;
            continue;
        }
        generations.insert(root.id);
        if (root.id == root.parent) {
            mixed = true;
            mixed_record = record;
            break;
        }
    }

    // Stop the child and reap it before reporting, so a failing assertion
    // cannot leave it unsharing behind the case.
    close(go[1]);
    close(report[0]);
    int status = 0;
    bool reaped = false;
    for (int i = 0; i < kPollTimeoutMs / 10; ++i) {
        if (waitpid(child, &status, WNOHANG) == child) {
            reaped = true;
            break;
        }
        usleep(10000);
    }
    ASSERT_TRUE(reaped) << "the child did not stop";
    ASSERT_TRUE(WIFEXITED(status)) << "the child did not exit normally";
    // Reap before reporting and disarm the guard last, so a failed assertion
    // above still kills a child that is still taking mount namespaces.
    child_guard.Disarm();
    const int code = WEXITSTATUS(status);
    if (code != 0) {
        GTEST_SKIP() << "the child could not keep taking mount namespaces: errno=" << code;
    }

    EXPECT_FALSE(read_failed) << "a read of " << path << " failed: errno=" << failed_errno;
    EXPECT_FALSE(mixed) << "a read mixed the mount namespace of one generation with the root of "
                           "the next:\n"
                        << mixed_record;
    if (!read_failed && !mixed && generations.size() < 2) {
        // Whether the reader meets more than one generation is up to the
        // scheduler, so a run that only ever saw one generation says nothing
        // about the property and is not a failure of it.
        GTEST_SKIP() << "the reader never saw the child change mount namespace (" << reads
                     << " reads, " << empty_records << " of them without a record)";
    }
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
