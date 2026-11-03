#include <gtest/gtest.h>
#include <cerrno>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <string>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {
class Fd {
 public:
    explicit Fd(int fd = -1) : fd_(fd) {}
    ~Fd() { reset(); }
    int get() const { return fd_; }
    void reset(int fd = -1) { if (fd_ >= 0) close(fd_); fd_ = fd; }
 private:
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int fd_;
};

int WaitChild(pid_t pid) {
    int status = 0;
    for (int i = 0; i < 500; ++i) {
        const pid_t result = waitpid(pid, &status, WNOHANG);
        if (result == pid) return WIFEXITED(status) ? WEXITSTATUS(status) : 125;
        if (result < 0 && errno != EINTR) return 126;
        usleep(10000);
    }
    kill(pid, SIGKILL);
    while (waitpid(pid, &status, 0) < 0 && errno == EINTR) {}
    return 124;
}

struct TargetInfo { int error; int fds[2]; };

class ProcPathRoot : public testing::Test {
 protected:
    void SetUp() override {
        char pattern[] = "/tmp/proc-path-root-XXXXXX";
        char* dir = mkdtemp(pattern);
        ASSERT_NE(nullptr, dir);
        base_ = dir;
        jail_ = base_ + "/jail";
        sibling_ = base_ + "/jail-extra";
        ASSERT_EQ(0, mkdir(jail_.c_str(), 0700));
        ASSERT_EQ(0, mkdir(sibling_.c_str(), 0700));
        paths_[0] = jail_ + "/proc-path-marker-in";
        paths_[1] = sibling_ + "/proc-path-marker-out";
        for (const auto& path : paths_) {
            Fd fd(open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600));
            ASSERT_GE(fd.get(), 0);
            ASSERT_EQ(0, ftruncate(fd.get(), 4096));
        }
        proc_.reset(open("/proc", O_RDONLY | O_DIRECTORY));
        ASSERT_GE(proc_.get(), 0);
    }
    void TearDown() override {
        // Releasing the pipe lets the target exit; a failed reader cannot leave
        // it, or its private bind mount, behind. WaitChild bounds every wait.
        stop_.reset();
        if (target_ > 0) { EXPECT_EQ(0, WaitChild(target_)); }
        proc_.reset();
        for (const auto& path : paths_) {
            if (!path.empty()) { EXPECT_EQ(0, unlink(path.c_str())); }
        }
        if (!sibling_.empty()) { EXPECT_EQ(0, rmdir(sibling_.c_str())); }
        if (!jail_.empty()) { EXPECT_EQ(0, rmdir(jail_.c_str())); }
        if (!base_.empty()) { EXPECT_EQ(0, rmdir(base_.c_str())); }
    }
    void StartTarget(bool separate_mount_namespace) {
        int ready[2];
        ASSERT_EQ(0, pipe(ready));
        Fd ready_read(ready[0]), ready_write(ready[1]);
        int stop[2];
        ASSERT_EQ(0, pipe(stop));
        Fd stop_read(stop[0]);
        stop_.reset(stop[1]);
        const pid_t parent = getpid();
        target_ = fork();
        ASSERT_GE(target_, 0);
        if (target_ == 0) {
            ready_read.reset();
            stop_.reset();
            alarm(15);
            TargetInfo info{0, {-1, -1}};
            if (prctl(PR_SET_PDEATHSIG, SIGKILL) || getppid() != parent) _exit(120);
            bool ok = true;
            if (separate_mount_namespace) {
                ok = unshare(CLONE_NEWNS) == 0 &&
                     mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr) == 0 &&
                     mount(jail_.c_str(), jail_.c_str(), nullptr, MS_BIND, nullptr) == 0;
            }
            for (int i = 0; ok && i < 2; ++i) {
                info.fds[i] = open(paths_[i].c_str(), O_RDONLY);
                ok = info.fds[i] >= 0 &&
                     mmap(nullptr, 4096, PROT_READ, MAP_PRIVATE, info.fds[i], 0) != MAP_FAILED;
            }
            if (ok) ok = chroot(jail_.c_str()) == 0 && chdir("/") == 0 &&
                         prctl(PR_SET_DUMPABLE, 1) == 0;
            if (!ok) info.error = errno ? errno : EIO;
            if (write(ready_write.get(), &info, sizeof(info)) != sizeof(info)) _exit(121);
            ready_write.reset();
            if (!ok) _exit(122);
            char byte;
            ssize_t count;
            do { count = read(stop_read.get(), &byte, 1); } while (count < 0 && errno == EINTR);
            _exit(count >= 0 ? 0 : 123);
        }
        ready_write.reset();
        stop_read.reset();
        pollfd event{ready_read.get(), POLLIN, 0};
        ASSERT_EQ(1, poll(&event, 1, 5000)) << "target readiness timed out";
        ASSERT_EQ(static_cast<ssize_t>(sizeof(info_)), read(ready_read.get(), &info_, sizeof(info_)));
        ASSERT_EQ(0, info_.error) << "target setup errno; unsupported operations must fail";
    }
    bool CheckPaths(const std::string& expected_in, const std::string& expected_out) {
        const std::string expected[2] = {expected_in, expected_out};
        bool links_ok = true;
        for (int i = 0; i < 2; ++i) {
            const std::string name = std::to_string(target_) + "/fd/" + std::to_string(info_.fds[i]);
            char buffer[8192];
            ssize_t n = readlinkat(proc_.get(), name.c_str(), buffer, sizeof(buffer));
            if (n < 0 || std::string(buffer, n) != expected[i]) {
                fprintf(stderr, "proc fd path mismatch: item=%d errno=%d\n", i, n < 0 ? errno : 0);
                links_ok = false;
            }
        }
        const std::string name = std::to_string(target_) + "/maps";
        Fd fd(openat(proc_.get(), name.c_str(), O_RDONLY));
        if (fd.get() < 0) return false;
        std::string maps;
        char buffer[4096];
        for (;;) {
            ssize_t n = read(fd.get(), buffer, sizeof(buffer));
            if (n < 0 && errno == EINTR) continue;
            if (n < 0) return false;
            if (n == 0) break;
            maps.append(buffer, n);
            if (maps.size() > 1024 * 1024) return false;
        }
        int seen[2] = {0, 0};
        for (size_t start = 0; start < maps.size();) {
            const size_t end = maps.find('\n', start);
            if (end == std::string::npos) return false;
            const std::string line = maps.substr(start, end - start);
            start = end + 1;
            for (int i = 0; i < 2; ++i) {
                const char* leaf = i == 0 ? "/proc-path-marker-in" : "/proc-path-marker-out";
                if (line.find(leaf) == std::string::npos) continue;
                const size_t path = line.find('/');
                if (path == std::string::npos || line.substr(path) != expected[i]) {
                    fprintf(stderr, "proc maps path mismatch: item=%d\n", i);
                    return false;
                }
                ++seen[i];
            }
        }
        return links_ok && seen[0] == 1 && seen[1] == 1;
    }
    void Reader(const std::string& root, bool join_target,
                const std::string& expected_in, const std::string& expected_out) {
        const pid_t child = fork();
        ASSERT_GE(child, 0);
        if (child == 0) {
            stop_.reset();
            alarm(5);
            if (join_target) {
                const std::string name = std::to_string(target_) + "/ns/mnt";
                Fd ns(openat(proc_.get(), name.c_str(), O_RDONLY));
                if (ns.get() < 0 || setns(ns.get(), CLONE_NEWNS) != 0) _exit(110);
            }
            if (!root.empty() && (chroot(root.c_str()) != 0 || chdir("/") != 0)) _exit(111);
            _exit(CheckPaths(expected_in, expected_out) ? 0 : 112);
        }
        EXPECT_EQ(0, WaitChild(child)) << "reader setup/path assertion failed";
    }
    std::string base_, jail_, sibling_, paths_[2];
    Fd proc_, stop_;
    pid_t target_ = -1;
    TargetInfo info_{};
};

TEST_F(ProcPathRoot, SameNamespaceUsesReaderRootRatherThanTargetRoot) {
    ASSERT_NO_FATAL_FAILURE(StartTarget(false));
    Reader("", false, paths_[0], paths_[1]);
    Reader(jail_, false, "/proc-path-marker-in", paths_[1]);
}
TEST_F(ProcPathRoot, SiblingComponentPrefixIsNotAnAncestor) {
    ASSERT_NO_FATAL_FAILURE(StartTarget(false));
    Reader(jail_, false, "/proc-path-marker-in", paths_[1]);
    Reader(sibling_, false, paths_[0], "/proc-path-marker-out");
}
TEST_F(ProcPathRoot, SameRootTextInDifferentMountNamespacesIsNotIdentity) {
    ASSERT_NO_FATAL_FAILURE(StartTarget(true));
    Reader(jail_, false, paths_[0], paths_[1]);
}
TEST_F(ProcPathRoot, JoiningTargetMountNamespaceRestoresRootIdentity) {
    ASSERT_NO_FATAL_FAILURE(StartTarget(true));
    Reader(jail_, true, "/proc-path-marker-in", paths_[1]);
}

// These fixtures use an independently owned child namespace so a failed bind
// or read cannot alter the runner's mount view. Cleanup remains in the parent.
class EscapedFiles {
 public:
    explicit EscapedFiles(const std::string& base)
        : source(base + "/escape-source"), view(base + "/escape-view"),
          leaf(source + "/leaf"), moved(base + "/escaped-leaf") {}
    ~EscapedFiles() {
        for (const auto& path : {leaf, moved}) {
            const int result = unlink(path.c_str());
            EXPECT_TRUE(result == 0 || errno == ENOENT);
        }
        for (const auto& path : {view, source}) {
            const int result = rmdir(path.c_str());
            EXPECT_TRUE(result == 0 || errno == ENOENT);
        }
    }
    std::string source, view, leaf, moved;
};

bool EscapedReadback(const EscapedFiles& paths, bool deleted) {
    if (unshare(CLONE_NEWNS) != 0 ||
        mount(nullptr, "/", nullptr, MS_PRIVATE | MS_REC, nullptr) != 0 ||
        mount(paths.source.c_str(), paths.view.c_str(), nullptr, MS_BIND, nullptr) != 0)
        return false;
    Fd fd(open((paths.view + "/leaf").c_str(), O_RDONLY));
    if (fd.get() < 0) return false;
    void* memory = mmap(nullptr, 4096, PROT_READ, MAP_PRIVATE, fd.get(), 0);
    if (memory == MAP_FAILED) return false;
    // The target dentry moves while the file still carries the bind mount.
    if (rename(paths.leaf.c_str(), paths.moved.c_str()) != 0 ||
        (deleted && unlink(paths.moved.c_str()) != 0)) return false;
    const std::string expected = deleted ? "/ (deleted)" : "/";
    char link[8192];
    const std::string fdname = "/proc/self/fd/" + std::to_string(fd.get());
    ssize_t n = readlink(fdname.c_str(), link, sizeof(link));
    const bool link_ok = n >= 0 && std::string(link, n) == expected;
    FILE* maps = fopen("/proc/self/maps", "r");
    if (!maps) return false;
    char* line = nullptr;
    size_t capacity = 0, total = 0;
    bool found = false, map_ok = false;
    while (getline(&line, &capacity, maps) >= 0) {
        total += strlen(line);
        if (total > 1024 * 1024) break;
        unsigned long long start, end;
        if (sscanf(line, "%llx-%llx", &start, &end) != 2) break;
        const auto address = reinterpret_cast<uintptr_t>(memory);
        if (start <= address && address < end) {
            found = true;
            const char* path = strchr(line, '/');
            map_ok = path && std::string(path) == expected + "\n";
            break;
        }
    }
    free(line);
    fclose(maps);
    const bool unmapped = munmap(memory, 4096) == 0;
    return link_ok && found && map_ok && unmapped;
}

void CheckEscaped(const std::string& base, bool deleted) {
    EscapedFiles paths(base);
    ASSERT_EQ(0, mkdir(paths.source.c_str(), 0700));
    ASSERT_EQ(0, mkdir(paths.view.c_str(), 0700));
    Fd file(open(paths.leaf.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600));
    ASSERT_GE(file.get(), 0);
    ASSERT_EQ(0, ftruncate(file.get(), 4096));
    const pid_t parent = getpid();
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        alarm(5);
        if (prctl(PR_SET_PDEATHSIG, SIGKILL) || getppid() != parent) _exit(120);
        _exit(EscapedReadback(paths, deleted) ? 0 : 112);
    }
    EXPECT_EQ(0, WaitChild(child)) << "escaped bind fd/maps path mismatch";
}
TEST_F(ProcPathRoot, RenamedOutsideBindRootUsesProcRootFallback) {
    CheckEscaped(base_, false);
}
TEST_F(ProcPathRoot, DeletedOutsideBindRootRetainsDeletedSuffix) {
    CheckEscaped(base_, true);
}
}  // namespace
int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
