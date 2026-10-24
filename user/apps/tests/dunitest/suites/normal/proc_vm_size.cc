// Linux 6.6 proc task_mem(): VmSize counts virtual mappings, including untouched
// ranges. Helpers use fixed buffers so observing the process does not
// change its address space through stdio or C++ heap allocation.
#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

class Fd {
public:
    explicit Fd(int fd) : fd_(fd) {}
    ~Fd() { if (fd_ >= 0) close(fd_); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return fd_; }
private:
    int fd_;
};

class Mapping {
public:
    explicit Mapping(size_t size) : size_(size), ptr_(
        mmap(nullptr, size, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0)) {}
    ~Mapping() { if (ptr_ != MAP_FAILED) munmap(ptr_, size_); }
    void* get() const { return ptr_; }
private:
    size_t size_;
    void* ptr_;
};

// A read may be short, and interrupted reads must not truncate the snapshot.
bool ReadFd(int fd, char* buffer, size_t capacity) {
    size_t used = 0;
    while (used + 1 < capacity) {
        ssize_t count = read(fd, buffer + used, capacity - used - 1);
        if (count < 0 && errno == EINTR) continue;
        if (count < 0) return false;
        if (count == 0) { buffer[used] = 0; return true; }
        used += static_cast<size_t>(count);
    }
    return false;
}

bool ReadFile(const char* path, char* buffer, size_t capacity) {
    Fd fd(open(path, O_RDONLY));
    return fd.get() >= 0 && ReadFd(fd.get(), buffer, capacity);
}

bool ParseVmSize(const char* text, size_t* bytes) {
    for (const char* line = text; line && *line; ) {
        if (strncmp(line, "VmSize:", 7) == 0) {
            const char* value = line + 7;
            while (*value == ' ' || *value == '\t') ++value;
            char* end = nullptr;
            errno = 0;
            unsigned long long kb = strtoull(value, &end, 10);
            if (end == value || errno || kb > SIZE_MAX / 1024) return false;
            while (*end == ' ' || *end == '\t') ++end;
            if (strncmp(end, "kB", 2) != 0 ||
                (end[2] != '\n' && end[2] != 0)) return false;
            *bytes = static_cast<size_t>(kb) * 1024;
            return true;
        }
        line = strchr(line, '\n');
        if (line) ++line;
    }
    return false;
}

bool VmSize(size_t* bytes, pid_t pid = 0) {
    char path[80];
    if (pid) snprintf(path, sizeof(path), "/proc/%d/status", pid);
    else strcpy(path, "/proc/self/status");
    char data[16384];
    return ReadFile(path, data, sizeof(data)) && ParseVmSize(data, bytes);
}

bool Transfer(int fd, void* data, size_t length, bool writing) {
    auto* p = static_cast<char*>(data);
    while (length) {
        if (!writing) {
            pollfd event{fd, POLLIN, 0};
            int ready;
            do { ready = poll(&event, 1, 5000); } while (ready < 0 && errno == EINTR);
            if (ready <= 0) return false;
        }
        ssize_t n = writing ? write(fd, p, length) : read(fd, p, length);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return false;
        p += n;
        length -= static_cast<size_t>(n);
    }
    return true;
}

void ExpectChildSuccess(pid_t pid) {
    int status = 0;
    ASSERT_EQ(waitpid(pid, &status, 0), pid);
    ASSERT_TRUE(WIFEXITED(status)) << status;
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

TEST(ProcVmSize, MatchesStatAndStatm) {
    // Resolve libc page-size discovery before taking the first snapshot.
    const size_t page = sysconf(_SC_PAGESIZE);
    size_t size = 0;
    ASSERT_TRUE(VmSize(&size));
    ASSERT_GT(size, 0u);
    char data[16384];
    ASSERT_TRUE(ReadFile("/proc/self/statm", data, sizeof(data)));
    char* end = nullptr;
    const unsigned long long pages = strtoull(data, &end, 10);
    ASSERT_NE(end, data);
    EXPECT_EQ(size, pages * page);

    ASSERT_TRUE(ReadFile("/proc/self/stat", data, sizeof(data)));
    char* field = strrchr(data, ')');
    ASSERT_NE(field, nullptr);
    field += 1;
    // Fields 3..22 precede vsize (field 23), including the nonnumeric state.
    for (int i = 3; i < 23; ++i) {
        while (*field == ' ') ++field;
        while (*field && *field != ' ') ++field;
        ASSERT_NE(*field, 0);
    }
    EXPECT_EQ(size, strtoull(field, &end, 10));
    EXPECT_NE(end, field);
}

TEST(ProcVmSize, UntouchedMappingsSplitsAndPartialUnmap) {
    const size_t page = sysconf(_SC_PAGESIZE);
    size_t before = 0, after = 0;
    ASSERT_TRUE(VmSize(&before));
    Mapping mapping(3 * page - 1);
    ASSERT_NE(mapping.get(), MAP_FAILED);
    ASSERT_TRUE(VmSize(&after));
    EXPECT_EQ(after, before + 3 * page);
    char* middle = static_cast<char*>(mapping.get()) + page;
    ASSERT_EQ(mprotect(middle, page, PROT_READ), 0);
    ASSERT_TRUE(VmSize(&after));
    EXPECT_EQ(after, before + 3 * page);
    ASSERT_EQ(munmap(middle, page), 0);
    ASSERT_TRUE(VmSize(&after));
    EXPECT_EQ(after, before + 2 * page);
}

TEST(ProcVmSize, FixedReplacementAndFailedMappingDoNotGrow) {
    const size_t page = sysconf(_SC_PAGESIZE);
    Mapping mapping(3 * page);
    ASSERT_NE(mapping.get(), MAP_FAILED);
    size_t before = 0, after = 0;
    ASSERT_TRUE(VmSize(&before));
    void* middle = static_cast<char*>(mapping.get()) + page;
    ASSERT_EQ(mmap(middle, page, PROT_READ,
                   MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0), middle);
    ASSERT_TRUE(VmSize(&after));
    EXPECT_EQ(after, before);
    errno = 0;
    EXPECT_EQ(mmap(static_cast<char*>(middle) + 1, page, PROT_NONE,
                   MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0), MAP_FAILED);
    EXPECT_EQ(errno, EINVAL);
    ASSERT_TRUE(VmSize(&after));
    EXPECT_EQ(after, before);
}

TEST(ProcVmSize, RawBrkHonorsDataLimitAndTracksGrowthAndShrink) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (!child) {
        const size_t page = sysconf(_SC_PAGESIZE);
        const auto old = static_cast<unsigned long>(syscall(SYS_brk, 0));
        const auto grown = (old + page - 1) / page * page + 4 * page;
        size_t before = 0, after = 0, restored = 0;
        if (!VmSize(&before)) _exit(1);
        rlimit original{};
        if (getrlimit(RLIMIT_DATA, &original)) _exit(6);
        rlimit blocked = original;
        blocked.rlim_cur = 0;
        if (setrlimit(RLIMIT_DATA, &blocked)) _exit(7);
        if (static_cast<unsigned long>(syscall(SYS_brk, grown)) != old) _exit(8);
        if (!VmSize(&after) || after != before) _exit(9);
        // Restore only the inherited soft limit; never raise the hard limit.
        if (setrlimit(RLIMIT_DATA, &original)) _exit(10);
        if (static_cast<unsigned long>(syscall(SYS_brk, grown)) != grown) _exit(2);
        if (!VmSize(&after) || after != before + 4 * page) _exit(3);
        if (static_cast<unsigned long>(syscall(SYS_brk, old)) != old) _exit(4);
        if (!VmSize(&restored) || restored != before) _exit(5);
        _exit(0);
    }
    ExpectChildSuccess(child);
}

TEST(ProcVmSize, ForkKeepsIndependentAccounting) {
    const size_t page = sysconf(_SC_PAGESIZE);
    Mapping mapping(4 * page);
    ASSERT_NE(mapping.get(), MAP_FAILED);
    size_t parent = 0;
    ASSERT_TRUE(VmSize(&parent));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (!child) {
        size_t before = 0, after = 0;
        if (!VmSize(&before) || before != parent) _exit(1);
        if (munmap(mapping.get(), 4 * page)) _exit(2);
        if (!VmSize(&after) || after != before - 4 * page) _exit(3);
        _exit(0);
    }
    ExpectChildSuccess(child);
    size_t after = 0;
    ASSERT_TRUE(VmSize(&after));
    EXPECT_EQ(after, parent);
}

struct ThreadResult { bool ok = false; };
void* CheckThread(void* arg) {
    auto* result = static_cast<ThreadResult*>(arg);
    char path[100], text[16384];
    snprintf(path, sizeof(path), "/proc/self/task/%ld/status", syscall(SYS_gettid));
    size_t thread = 0, process = 0;
    result->ok = ReadFile(path, text, sizeof(text)) && ParseVmSize(text, &thread) &&
                 VmSize(&process) && thread == process;
    return nullptr;
}

TEST(ProcVmSize, ThreadSharesAddressSpaceView) {
    pthread_t thread;
    ThreadResult result;
    ASSERT_EQ(pthread_create(&thread, nullptr, CheckThread, &result), 0);
    ASSERT_EQ(pthread_join(thread, nullptr), 0);
    EXPECT_TRUE(result.ok);
}

TEST(ProcVmSize, SnapshotStaysAtEofAndRewindRefreshes) {
    Fd fd(open("/proc/self/status", O_RDONLY));
    ASSERT_GE(fd.get(), 0);
    char data[16384];
    size_t before = 0, after = 0;
    ASSERT_TRUE(ReadFd(fd.get(), data, sizeof(data)));
    ASSERT_TRUE(ParseVmSize(data, &before));
    const size_t page = sysconf(_SC_PAGESIZE);
    Mapping mapping(3 * page);
    ASSERT_NE(mapping.get(), MAP_FAILED);
    EXPECT_EQ(read(fd.get(), data, 1), 0);
    ASSERT_EQ(lseek(fd.get(), 0, SEEK_SET), 0);
    ASSERT_TRUE(ReadFd(fd.get(), data, sizeof(data)));
    ASSERT_TRUE(ParseVmSize(data, &after));
    EXPECT_EQ(after, before + 3 * page);
}

// The exec helper reports its own stable mm, then waits for the parent. The
// parent uses a status fd opened before exec: status follows the task's new mm.
constexpr char kExecArg[] = "--proc-vm-size-exec";
int ExecHelper(int report, int control) {
    size_t size = 0;
    char go;
    return VmSize(&size) && Transfer(report, &size, sizeof(size), true) &&
           Transfer(control, &go, 1, false) ? 0 : 1;
}

TEST(ProcVmSize, StatusTracksExecAndOmitsMemoryAfterExit) {
    int report[2], control[2];
    ASSERT_EQ(pipe(report), 0);
    Fd report_read(report[0]), report_write(report[1]);
    ASSERT_EQ(pipe(control), 0);
    Fd control_read(control[0]), control_write(control[1]);
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (!child) {
        close(report[0]);
        close(control[1]);
        Mapping old_mapping(8u << 20);
        if (old_mapping.get() == MAP_FAILED) _exit(1);
        char ready = 1, go;
        if (!Transfer(report[1], &ready, 1, true) ||
            !Transfer(control[0], &go, 1, false)) _exit(2);
        char report_arg[32], control_arg[32];
        snprintf(report_arg, sizeof(report_arg), "%d", report[1]);
        snprintf(control_arg, sizeof(control_arg), "%d", control[0]);
        execl("/proc/self/exe", "/proc/self/exe", kExecArg, report_arg, control_arg,
              static_cast<char*>(nullptr));
        _exit(3);
    }
    // Always reap the helper, including fatal assertion paths.
    struct Reap {
        pid_t pid;
        ~Reap() { kill(pid, SIGKILL); while (waitpid(pid, nullptr, 0) < 0 && errno == EINTR) {} }
    } reap{child};
    char go = 1;
    ASSERT_TRUE(Transfer(report[0], &go, 1, false));
    char path[80];
    snprintf(path, sizeof(path), "/proc/%d/status", child);
    Fd status(open(path, O_RDONLY));
    ASSERT_GE(status.get(), 0);
    ASSERT_TRUE(Transfer(control[1], &go, 1, true));
    size_t expected = 0, observed = 0;
    ASSERT_TRUE(Transfer(report[0], &expected, sizeof(expected), false));
    char data[16384];
    ASSERT_TRUE(ReadFd(status.get(), data, sizeof(data)));
    ASSERT_TRUE(ParseVmSize(data, &observed));
    EXPECT_EQ(observed, expected);
    ASSERT_TRUE(Transfer(control[1], &go, 1, true));
    siginfo_t info{};
    ASSERT_EQ(waitid(P_PID, child, &info, WEXITED | WNOWAIT), 0);
    EXPECT_EQ(info.si_code, CLD_EXITED);
    EXPECT_EQ(info.si_status, 0);
    ASSERT_TRUE(ReadFile(path, data, sizeof(data)));
    EXPECT_FALSE(ParseVmSize(data, &observed));
}

} // namespace

int main(int argc, char** argv) {
    if (argc == 4 && strcmp(argv[1], kExecArg) == 0)
        return ExecHelper(atoi(argv[2]), atoi(argv[3]));
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
