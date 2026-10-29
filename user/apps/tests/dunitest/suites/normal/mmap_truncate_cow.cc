#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <string>

namespace {

size_t PageSize() {
    const long ps = sysconf(_SC_PAGESIZE);
    return ps > 0 ? static_cast<size_t>(ps) : 4096;
}

enum class BoundedReap { Exited, Deadline, LostChild, Error };

BoundedReap ReapWithin(pid_t pid, int* status, int64_t duration_ns) {
    timespec now{};
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return BoundedReap::Error;
    const int64_t deadline = static_cast<int64_t>(now.tv_sec) * 1000000000 + now.tv_nsec + duration_ns;
    for (;;) {
        const pid_t result = waitpid(pid, status, WNOHANG);
        if (result == pid) return BoundedReap::Exited;
        if (result < 0 && errno != EINTR) {
            return errno == ECHILD ? BoundedReap::LostChild : BoundedReap::Error;
        }
        if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return BoundedReap::Error;
        if (static_cast<int64_t>(now.tv_sec) * 1000000000 + now.tv_nsec >= deadline) {
            return BoundedReap::Deadline;
        }
        const timespec pause{0, 1000000};
        nanosleep(&pause, nullptr);
    }
}

class TempFile {
  public:
    TempFile() {
        char tmpl[] = "/tmp/dunitest_mmap_truncate_cow_XXXXXX";
        fd_ = mkstemp(tmpl);
        if (fd_ >= 0) {
            path_ = tmpl;
        }
    }

    ~TempFile() {
        if (fd_ >= 0) {
            close(fd_);
        }
        if (!path_.empty()) {
            unlink(path_.c_str());
        }
    }

    TempFile(const TempFile&) = delete;
    TempFile& operator=(const TempFile&) = delete;

    bool valid() const {
        return fd_ >= 0;
    }

    int fd() const {
        return fd_;
    }

  private:
    std::string path_;
    int fd_ = -1;
};

void ExpectChildDiesBySignal(int signal, void (*fn)()) {
    const pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        fn();
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0))
        << "waitpid failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_TRUE(WIFSIGNALED(status)) << "child exited without signal, status=" << status;
    EXPECT_EQ(signal, WTERMSIG(status)) << "unexpected signal, status=" << status;
}

volatile char* g_mapping = nullptr;

void ReadMappedByte() {
    const volatile char byte = g_mapping[0];
    (void)byte;
}

}  // namespace

TEST(MmapTruncateCow, PrivateCowPageIsInvalidatedAfterTruncateToZero) {
    const size_t ps = PageSize();
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: errno=" << errno << " (" << strerror(errno)
                              << ")";
    ASSERT_EQ(0, ftruncate(file.fd(), static_cast<off_t>(ps)))
        << "ftruncate to page failed: errno=" << errno << " (" << strerror(errno) << ")";

    void* mapping = mmap(nullptr, ps, PROT_READ | PROT_WRITE, MAP_PRIVATE, file.fd(), 0);
    ASSERT_NE(MAP_FAILED, mapping) << "mmap failed: errno=" << errno << " (" << strerror(errno)
                                   << ")";

    memset(mapping, 'a', ps);
    ASSERT_EQ(0, ftruncate(file.fd(), 0))
        << "ftruncate to zero failed: errno=" << errno << " (" << strerror(errno) << ")";

    g_mapping = static_cast<volatile char*>(mapping);
    ExpectChildDiesBySignal(SIGBUS, ReadMappedByte);

    ASSERT_EQ(0, munmap(mapping, ps)) << "munmap failed: errno=" << errno << " ("
                                      << strerror(errno) << ")";
    g_mapping = nullptr;
}

TEST(MmapTruncateCow, PartialPageTruncateKeepsContainingPageAndInvalidatesFollowingCowPage) {
    const size_t ps = PageSize();
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: errno=" << errno << " (" << strerror(errno)
                              << ")";
    ASSERT_EQ(0, ftruncate(file.fd(), static_cast<off_t>(ps * 2)))
        << "ftruncate to two pages failed: errno=" << errno << " (" << strerror(errno) << ")";

    void* mapping = mmap(nullptr, ps * 2, PROT_READ | PROT_WRITE, MAP_PRIVATE, file.fd(), 0);
    ASSERT_NE(MAP_FAILED, mapping) << "mmap failed: errno=" << errno << " (" << strerror(errno)
                                   << ")";

    memset(mapping, 'b', ps * 2);
    ASSERT_EQ(0, ftruncate(file.fd(), static_cast<off_t>(ps / 2)))
        << "partial ftruncate failed: errno=" << errno << " (" << strerror(errno) << ")";

    auto* bytes = static_cast<volatile char*>(mapping);
    EXPECT_EQ('b', bytes[0]);

    g_mapping = bytes + ps;
    ExpectChildDiesBySignal(SIGBUS, ReadMappedByte);

    ASSERT_EQ(0, munmap(mapping, ps * 2)) << "munmap failed: errno=" << errno << " ("
                                          << strerror(errno) << ")";
    g_mapping = nullptr;
}

TEST(MmapTruncateCow, LastPrivateFileCowMappingSurvivesParentUnmap) {
    // The child inherits an anonymous COW page in a file-backed VMA. After
    // parent unmap, its write replaces the last mapping of that old page.
    const size_t ps = PageSize();
    const size_t length = ps * 256;
    TempFile file;
    ASSERT_TRUE(file.valid());
    ASSERT_EQ(0, ftruncate(file.fd(), length));
    auto* bytes = static_cast<volatile char*>(
        mmap(nullptr, length, PROT_READ | PROT_WRITE, MAP_PRIVATE, file.fd(), 0));
    ASSERT_NE(MAP_FAILED, const_cast<char*>(bytes));
    for (size_t offset = 0; offset < length; offset += ps) {
        bytes[offset] = 'a';
    }
    int ready[2];
    ASSERT_EQ(0, pipe(ready));
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(ready[1]);
        char token;
        if (read(ready[0], &token, 1) != 1) _exit(91);
        close(ready[0]);
        for (size_t offset = 0; offset < length; offset += ps) {
            if (bytes[offset] != 'a') _exit(92);
            bytes[offset] = 'b';
            if (bytes[offset] != 'b') _exit(93);
        }
        if (munmap(const_cast<char*>(bytes), length) != 0) _exit(94);
        _exit(0);
    }
    close(ready[0]);
    EXPECT_EQ(0, munmap(const_cast<char*>(bytes), length));
    EXPECT_EQ(1, write(ready[1], "x", 1));
    close(ready[1]);
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    char byte = 1;
    ASSERT_EQ(1, pread(file.fd(), &byte, 1, 0));
    EXPECT_EQ(0, byte);  // Neither private write may change the backing file.
}

TEST(MmapTruncateCow, AnonymousChildCowDoesNotInheritParentMlock) {
    const size_t length = PageSize() * 4;
    auto* bytes = static_cast<volatile char*>(
        mmap(nullptr, length, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, const_cast<char*>(bytes));
    ASSERT_EQ(0, mlock(const_cast<char*>(bytes), length));
    bytes[0] = 'p';
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        // Linux does not inherit VM_LOCKED across fork. This COW page must
        // acquire the child's policy, not the locked source page's policy.
        if (bytes[0] != 'p') _exit(95);
        for (size_t offset = 0; offset < length; offset += PageSize()) {
            bytes[offset] = 'c';
        }
        if (munmap(const_cast<char*>(bytes), length) != 0) _exit(96);
        _exit(0);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_EQ('p', bytes[0]);
    EXPECT_EQ(0, munlock(const_cast<char*>(bytes), length));
    EXPECT_EQ(0, munmap(const_cast<char*>(bytes), length));
}

TEST(MmapTruncateCow, SharedManyMappingsSplitMlockForkAndDontUnmap) {
    // Keep the test runner out of the shared mm. The worker owns its process
    // group until reaped, so a timed-out fork/fault cannot strand the suite.
    const pid_t worker = fork();
    ASSERT_GE(worker, 0);
    if (worker == 0) {
        const auto regression = []() -> int {
        if (setpgid(0, 0) != 0) return 101;
        const size_t ps = PageSize();
        const size_t length = 3 * ps;
        TempFile file;
        if (!file.valid() || ftruncate(file.fd(), length) != 0) return 102;
        volatile char* maps[3];
        for (auto& mapping : maps) {
            mapping = static_cast<volatile char*>(
                mmap(nullptr, length, PROT_READ | PROT_WRITE, MAP_SHARED, file.fd(), 0));
            if (mapping == MAP_FAILED) return 103;
        }
        for (size_t page = 0; page < 3; ++page) {
            maps[0][page * ps] = static_cast<char>('a' + page);
            for (size_t alias = 1; alias < 3; ++alias) {
                if (maps[alias][page * ps] != static_cast<char>('a' + page)) return 104;
            }
        }
        // Three genuine VMAs reference each PFN. The third mlock source also
        // exercises the heap-backed mlock subset, not a synthetic map count.
        for (auto* mapping : maps) {
            if (mlock(const_cast<char*>(mapping), length) != 0) return 105;
        }
        if (mprotect(const_cast<char*>(maps[0] + ps), ps, PROT_READ) != 0) return 106;
        if (mprotect(const_cast<char*>(maps[1] + ps), ps, PROT_READ) != 0) return 107;
        if (munlock(const_cast<char*>(maps[2] + ps), ps) != 0) return 108;

        const pid_t child = fork();
        if (child < 0) return 109;
        if (child == 0) {
            maps[2][2 * ps] = 'f';
            _exit(maps[0][2 * ps] == 'f' && maps[1][2 * ps] == 'f' ? 0 : 110);
        }
        int status = 0;
        const auto child_result = ReapWithin(child, &status, 2000000000LL);
        if (child_result != BoundedReap::Exited) {
            if (child_result == BoundedReap::LostChild) return 111;
            kill(child, SIGKILL);
            // Cleanup is bounded too; never use a blocking wait after failure.
            (void)ReapWithin(child, &status, 2000000000LL);
            return 111;
        }
        if (!WIFEXITED(status) || WEXITSTATUS(status) != 0 || maps[0][2 * ps] != 'f') return 112;

        auto* moved = static_cast<volatile char*>(
            mremap(const_cast<char*>(maps[2]), ps, ps, MREMAP_MAYMOVE | MREMAP_DONTUNMAP));
        if (moved == MAP_FAILED || moved == maps[2] || moved[0] != 'a') return 113;
        moved[0] = 'z';
        // Linux 6.6 leaves the shared-file source VMA in place. Its next fault
        // reattaches the SAME backing page; only private anonymous sources
        // have zero-fill semantics. New target/fork VMAs have new lineages.
        if (maps[0][0] != 'z' || maps[1][0] != 'z' || maps[2][0] != 'z') return 114;
        if (munmap(const_cast<char*>(moved), ps) != 0) return 115;
        for (auto* mapping : maps) {
            if (munlock(const_cast<char*>(mapping), length) != 0 ||
                munmap(const_cast<char*>(mapping), length) != 0) return 116;
        }
        return 0;
        };
        _exit(regression());
    }
    const int group_result = setpgid(worker, worker);
    const int group_error = errno;
    const bool group_ready = group_result == 0 || group_error == EACCES || group_error == ESRCH;
    timespec start{};
    const bool have_clock = clock_gettime(CLOCK_MONOTONIC, &start) == 0;
    const int64_t deadline = static_cast<int64_t>(start.tv_sec) * 1000000000 + start.tv_nsec +
                             8000000000LL;
    int status = 0;
    bool reaped = false;
    bool lost_child = false;
    bool timed_out = !have_clock || !group_ready;
    while (!timed_out) {
        const pid_t result = waitpid(worker, &status, WNOHANG);
        if (result == worker) { reaped = true; break; }
        if (result < 0 && errno != EINTR) { lost_child = errno == ECHILD; timed_out = true; break; }
        timespec now{};
        if (clock_gettime(CLOCK_MONOTONIC, &now) != 0 ||
            static_cast<int64_t>(now.tv_sec) * 1000000000 + now.tv_nsec >= deadline) {
            timed_out = true;
            break;
        }
        const timespec pause{0, 1000000};
        nanosleep(&pause, nullptr);
    }
    if (!reaped && !lost_child) {
        if (group_ready) kill(-worker, SIGKILL);  // Owned by the unreaped leader.
        kill(worker, SIGKILL);
        reaped = ReapWithin(worker, &status, 2000000000LL) == BoundedReap::Exited;
    }
    ASSERT_TRUE(have_clock);
    ASSERT_TRUE(group_ready) << "setpgid errno=" << group_error;
    ASSERT_FALSE(timed_out) << "worker exceeded absolute deadline";
    ASSERT_TRUE(reaped);
    ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status)) << "worker stage=" << WEXITSTATUS(status);
}

TEST(MmapTruncateCow, FixedReplacementRetiresMoreThanFourVmas) {
    const pid_t worker = fork();
    ASSERT_GE(worker, 0);
    if (worker == 0) {
        const int result = [] {
            TempFile file;
            if (file.fd() < 0) return 1;
            const size_t page = PageSize();
            constexpr size_t count = 12;
            if (ftruncate(file.fd(), page * count) != 0) return 2;
            char* target = static_cast<char*>(mmap(nullptr, page * count, PROT_NONE,
                                                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
            if (target == MAP_FAILED) return 3;
            for (int operation = 0; operation != 2; ++operation) {
                // Distinct offsets/protections create twelve real original
                // file VMAs, not one large VMA whose retirement fits four slots.
                for (size_t index = 0; index != count; ++index) {
                    const int protection = PROT_READ | ((index & 1) ? PROT_WRITE : 0);
                    void* mapped = mmap(target + page * index, page, protection,
                                        MAP_SHARED | MAP_FIXED, file.fd(), page * index);
                    if (mapped != target + page * index) return 4;
                    (void)*static_cast<volatile char*>(mapped);
                }
                if (operation == 0) {
                    void* replaced = mmap(target, page * count, PROT_READ | PROT_WRITE,
                                          MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
                    if (replaced != target) return 5;
                } else {
                    char* source = static_cast<char*>(mmap(nullptr, page, PROT_READ | PROT_WRITE,
                                                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
                    if (source == MAP_FAILED) return 6;
                    *source = 'R';
                    void* moved = mremap(source, page, page * count,
                                        MREMAP_MAYMOVE | MREMAP_FIXED, target);
                    if (moved != target) return 7;
                    if (*target != 'R') return 8;
                }
                target[page * count - 1] = 'T';
                if (target[page * count - 1] != 'T') return 9;
            }
            return munmap(target, page * count) == 0 ? 0 : 10;
        }();
        _exit(result);
    }
    int status = 0;
    const BoundedReap result = ReapWithin(worker, &status, 8000000000LL);
    if (result != BoundedReap::Exited && result != BoundedReap::LostChild) {
        kill(worker, SIGKILL);  // Unreaped, exclusively owned child identity.
        EXPECT_EQ(BoundedReap::Exited, ReapWithin(worker, &status, 2000000000LL));
    }
    ASSERT_EQ(BoundedReap::Exited, result);
    ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status)) << "worker stage=" << WEXITSTATUS(status);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
