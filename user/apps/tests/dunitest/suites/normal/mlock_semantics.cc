#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>

#ifndef SYS_mlock2
#define SYS_mlock2 325
#endif

#ifndef SYS_mremap
#define SYS_mremap 25
#endif

#ifndef MCL_ONFAULT
#define MCL_ONFAULT 4
#endif

#ifndef MLOCK_ONFAULT
#define MLOCK_ONFAULT 1
#endif

#ifndef MREMAP_MAYMOVE
#define MREMAP_MAYMOVE 1
#endif

#ifndef MREMAP_DONTUNMAP
#define MREMAP_DONTUNMAP 4
#endif

#ifndef MAP_FIXED_NOREPLACE
#define MAP_FIXED_NOREPLACE 0x100000
#endif

namespace {

size_t PageSize() {
    long ps = sysconf(_SC_PAGESIZE);
    return ps > 0 ? static_cast<size_t>(ps) : 4096;
}

bool IsResident(unsigned char v) {
    return (v & 1) != 0;
}

class ScopedMemlockLimit {
  public:
    ScopedMemlockLimit() : valid_(getrlimit(RLIMIT_MEMLOCK, &saved_) == 0) {}

    ~ScopedMemlockLimit() {
        if (valid_) {
            (void)setrlimit(RLIMIT_MEMLOCK, &saved_);
        }
    }

    bool valid() const {
        return valid_;
    }

    bool set_exact(rlim_t soft, rlim_t hard) {
        if (!valid_) {
            return false;
        }
        struct rlimit lim = {
            .rlim_cur = soft,
            .rlim_max = hard,
        };
        return setrlimit(RLIMIT_MEMLOCK, &lim) == 0;
    }

    bool set_bytes(size_t bytes) {
        if (!valid_) {
            return false;
        }

        rlim_t want = static_cast<rlim_t>(bytes);
        if (saved_.rlim_max != RLIM_INFINITY && want > saved_.rlim_max) {
            return false;
        }

        struct rlimit lim = saved_;
        lim.rlim_cur = want;
        return setrlimit(RLIMIT_MEMLOCK, &lim) == 0;
    }

    bool raise_to_hard_limit() {
        if (!valid_) {
            return false;
        }

        struct rlimit lim = saved_;
        lim.rlim_cur = lim.rlim_max;
        return setrlimit(RLIMIT_MEMLOCK, &lim) == 0;
    }

    rlim_t hard_limit() const {
        return saved_.rlim_max;
    }

  private:
    struct rlimit saved_ {};
    bool valid_ = false;
};

class TempFilePage {
  public:
    TempFilePage() {
        char tmpl[] = "/tmp/dunitest_mlock_XXXXXX";
        fd_ = mkstemp(tmpl);
        if (fd_ >= 0) {
            path_ = tmpl;
        }
    }

    ~TempFilePage() {
        if (fd_ >= 0) {
            close(fd_);
        }
        if (!path_.empty()) {
            unlink(path_.c_str());
        }
    }

    bool valid() const {
        return fd_ >= 0;
    }

    int fd() const {
        return fd_;
    }

    bool write_pattern(size_t bytes, unsigned char value) {
        if (fd_ < 0) {
            return false;
        }
        std::string buf(bytes, static_cast<char>(value));
        return write(fd_, buf.data(), buf.size()) == static_cast<ssize_t>(buf.size());
    }

  private:
    std::string path_;
    int fd_ = -1;
};

}  // namespace

TEST(Mlock, LinuxCompatibleArgumentValidation) {
    const size_t ps = PageSize();
    void* addr = mmap(nullptr, ps, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, addr) << "mmap failed: errno=" << errno << " (" << strerror(errno)
                                << ")";

    errno = 0;
    EXPECT_EQ(0, mlock(addr, 0));
    EXPECT_EQ(0, errno);

    errno = 0;
    EXPECT_EQ(0, munlock(addr, 0));
    EXPECT_EQ(0, errno);

    errno = 0;
    EXPECT_EQ(0, static_cast<int>(syscall(SYS_mlock2, addr, 0, 0)));
    EXPECT_EQ(0, errno);

    errno = 0;
    EXPECT_EQ(-1, static_cast<int>(syscall(SYS_mlock2, addr, ps, 0x80000000U)));
    EXPECT_EQ(EINVAL, errno);

    errno = 0;
    EXPECT_EQ(-1, mlockall(0));
    EXPECT_EQ(EINVAL, errno);

    errno = 0;
    EXPECT_EQ(-1, mlockall(MCL_ONFAULT));
    EXPECT_EQ(EINVAL, errno);

    EXPECT_EQ(0, munmap(addr, ps));
}

TEST(Mlock, RlimitZeroDeniedOrCapabilityBypassed) {
    const size_t ps = PageSize();
    ScopedMemlockLimit lim;
    ASSERT_TRUE(lim.valid()) << "getrlimit(RLIMIT_MEMLOCK) failed";

    void* addr = mmap(nullptr, ps, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, addr) << "mmap failed: errno=" << errno << " (" << strerror(errno)
                                << ")";

    ASSERT_TRUE(lim.set_exact(0, lim.hard_limit()))
        << "setrlimit failed: errno=" << errno << " (" << strerror(errno) << ")";

    errno = 0;
    int ret = mlock(addr, ps);
    ASSERT_TRUE((ret == -1 && errno == EPERM) || ret == 0)
        << "ret=" << ret << ", errno=" << errno << " (" << strerror(errno) << ")";
    if (ret == 0) {
        EXPECT_EQ(0, munlock(addr, ps));
    }

    EXPECT_EQ(0, munmap(addr, ps));
}

TEST(Mlock, PopulatesAnonymousPages) {
    const size_t ps = PageSize();
    const size_t len = ps * 2;
    ScopedMemlockLimit lim;
    ASSERT_TRUE(lim.valid());
    ASSERT_TRUE(lim.set_bytes(len));

    unsigned char vec[2] = {0, 0};
    auto* addr = static_cast<char*>(
        mmap(nullptr, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, addr) << "mmap failed: errno=" << errno << " (" << strerror(errno)
                                << ")";

    ASSERT_EQ(0, mlock(addr + 1, ps)) << "errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_EQ(0, mincore(addr, len, vec)) << "errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_TRUE(IsResident(vec[0]));
    EXPECT_TRUE(IsResident(vec[1]));
    EXPECT_EQ(0, munlock(addr + 1, ps));
    EXPECT_EQ(0, munmap(addr, len));
}

TEST(Mlock, PopulatesFileMappingBeforeFault) {
    const size_t ps = PageSize();
    ScopedMemlockLimit lim;
    ASSERT_TRUE(lim.valid());
    ASSERT_TRUE(lim.set_bytes(ps));

    TempFilePage file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: errno=" << errno << " (" << strerror(errno)
                              << ")";
    ASSERT_TRUE(file.write_pattern(ps, 0x5a));

    auto* addr = static_cast<char*>(mmap(nullptr, ps, PROT_READ, MAP_PRIVATE, file.fd(), 0));
    ASSERT_NE(MAP_FAILED, addr) << "mmap file failed: errno=" << errno << " (" << strerror(errno)
                                << ")";

    ASSERT_EQ(0, mlock(addr, ps)) << "errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_EQ(static_cast<char>(0x5a), addr[0]);
    EXPECT_EQ(0, munlock(addr, ps));
    EXPECT_EQ(0, munmap(addr, ps));
}

TEST(Mlock2, OnFaultDoesNotPrefault) {
    const size_t ps = PageSize();
    const size_t len = ps * 2;
    ScopedMemlockLimit lim;
    ASSERT_TRUE(lim.valid());
    ASSERT_TRUE(lim.set_bytes(len));

    auto* addr = static_cast<char*>(
        mmap(nullptr, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, addr);

    ASSERT_EQ(0, static_cast<int>(syscall(SYS_mlock2, addr, len, MLOCK_ONFAULT)))
        << "errno=" << errno << " (" << strerror(errno) << ")";

    unsigned char vec[2] = {0, 0};
    ASSERT_EQ(0, mincore(addr, len, vec));
    EXPECT_FALSE(IsResident(vec[0]));
    EXPECT_FALSE(IsResident(vec[1]));

    addr[0] = 1;
    vec[0] = vec[1] = 0;
    ASSERT_EQ(0, mincore(addr, len, vec));
    EXPECT_TRUE(IsResident(vec[0]));

    EXPECT_EQ(0, munlock(addr, len));
    EXPECT_EQ(0, munmap(addr, len));
}

TEST(MlockAll, FutureAppliesToNewMappings) {
    const size_t ps = PageSize();
    ScopedMemlockLimit lim;
    ASSERT_TRUE(lim.valid());
    if (!lim.raise_to_hard_limit()) {
        GTEST_SKIP() << "unable to raise RLIMIT_MEMLOCK to hard limit";
    }

    ASSERT_EQ(0, mlockall(MCL_FUTURE)) << "errno=" << errno << " (" << strerror(errno) << ")";

    void* fresh = mmap(nullptr, ps, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, fresh);

    errno = 0;
    EXPECT_EQ(-1, msync(fresh, ps, MS_INVALIDATE));
    EXPECT_EQ(EBUSY, errno);

    EXPECT_EQ(0, munlockall());
    EXPECT_EQ(0, munmap(fresh, ps));
}

TEST(MlockAll, CurrentWithProtNoneClearsStaleFuture) {
    const size_t ps = PageSize();
    ScopedMemlockLimit lim;
    ASSERT_TRUE(lim.valid());
    if (!lim.raise_to_hard_limit()) {
        GTEST_SKIP() << "unable to raise RLIMIT_MEMLOCK to hard limit";
    }

    void* guard = mmap(nullptr, ps, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, guard);

    ASSERT_EQ(0, mlockall(MCL_FUTURE)) << "errno=" << errno << " (" << strerror(errno) << ")";

    errno = 0;
    if (mlockall(MCL_CURRENT) == -1 && errno == ENOMEM) {
        munlockall();
        munmap(guard, ps);
        GTEST_SKIP() << "RLIMIT_MEMLOCK too small for mlockall(MCL_CURRENT)";
    }

    void* fresh = mmap(nullptr, ps, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, fresh);

    errno = 0;
    EXPECT_EQ(0, msync(fresh, ps, MS_INVALIDATE)) << "errno=" << errno << " ("
                                                  << strerror(errno) << ")";

    EXPECT_EQ(0, munmap(fresh, ps));
    EXPECT_EQ(0, munlockall());
    EXPECT_EQ(0, munmap(guard, ps));
}

TEST(MlockAll, FutureAppliesToBrkGrowth) {
    const size_t ps = PageSize();
    ScopedMemlockLimit lim;
    ASSERT_TRUE(lim.valid());
    if (!lim.raise_to_hard_limit()) {
        GTEST_SKIP() << "unable to raise RLIMIT_MEMLOCK to hard limit";
    }

    pid_t pid = fork();
    ASSERT_GE(pid, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (pid == 0) {
        void* old_brk = sbrk(0);
        if (old_brk == reinterpret_cast<void*>(-1)) {
            _exit(2);
        }

        void* baseline = sbrk(static_cast<intptr_t>(ps));
        if (baseline == reinterpret_cast<void*>(-1)) {
            _exit(errno == ENOMEM ? 77 : 3);
        }
        if (sbrk(-static_cast<intptr_t>(ps)) == reinterpret_cast<void*>(-1)) {
            _exit(4);
        }

        if (mlockall(MCL_FUTURE) != 0) {
            _exit(5);
        }

        void* grown = sbrk(static_cast<intptr_t>(ps));
        if (grown == reinterpret_cast<void*>(-1)) {
            _exit(1);
        }

        auto* p = static_cast<volatile char*>(grown);
        *p = 0x12;

        errno = 0;
        if (msync(grown, ps, MS_INVALIDATE) != -1 || errno != EBUSY) {
            _exit(6);
        }

        if (sbrk(-static_cast<intptr_t>(ps)) == reinterpret_cast<void*>(-1)) {
            _exit(7);
        }
        if (munlockall() != 0) {
            _exit(8);
        }
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(pid, waitpid(pid, &status, 0)) << "waitpid failed: errno=" << errno << " ("
                                             << strerror(errno) << ")";
    ASSERT_TRUE(WIFEXITED(status)) << "child terminated abnormally";
    if (WEXITSTATUS(status) == 77) {
        GTEST_SKIP() << "environment cannot grow brk by one page without mlockall";
    }
    EXPECT_EQ(0, WEXITSTATUS(status))
        << "child exit code=" << WEXITSTATUS(status)
        << " (1 means MCL_FUTURE brk growth failed after a successful baseline growth)";
}

TEST(Mlock, MapLockedBehavesAsLockedMapping) {
    const size_t ps = PageSize();
    ScopedMemlockLimit lim;
    ASSERT_TRUE(lim.valid());
    ASSERT_TRUE(lim.set_bytes(ps));

    unsigned char vec[1] = {0};
    void* addr = mmap(nullptr, ps, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS | MAP_LOCKED, -1, 0);
    ASSERT_NE(MAP_FAILED, addr) << "errno=" << errno << " (" << strerror(errno) << ")";

    ASSERT_EQ(0, mincore(addr, ps, vec));
    EXPECT_TRUE(IsResident(vec[0]));

    errno = 0;
    EXPECT_EQ(-1, msync(addr, ps, MS_INVALIDATE));
    EXPECT_EQ(EBUSY, errno);

    EXPECT_EQ(0, munmap(addr, ps));
}

TEST(Mremap, DontUnmapClearsSourceLock) {
    const size_t ps = PageSize();
    ScopedMemlockLimit lim;
    ASSERT_TRUE(lim.valid());
    ASSERT_TRUE(lim.set_bytes(ps * 4));

    auto* addr = static_cast<char*>(
        mmap(nullptr, ps, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, addr);
    ASSERT_EQ(0, mlock(addr, ps));

    errno = 0;
    void* moved =
        reinterpret_cast<void*>(syscall(SYS_mremap, addr, ps, ps, MREMAP_MAYMOVE | MREMAP_DONTUNMAP, 0));
    ASSERT_NE(MAP_FAILED, moved) << "errno=" << errno << " (" << strerror(errno) << ")";

    errno = 0;
    EXPECT_EQ(0, msync(addr, ps, MS_INVALIDATE)) << "errno=" << errno << " (" << strerror(errno)
                                                 << ")";

    errno = 0;
    EXPECT_EQ(-1, msync(moved, ps, MS_INVALIDATE));
    EXPECT_EQ(EBUSY, errno);

    EXPECT_EQ(0, munmap(moved, ps));
    EXPECT_EQ(0, munmap(addr, ps));
}

TEST(Mremap, DontUnmapPartialRangeClearsWholeSourceLock) {
    const size_t ps = PageSize();
    ScopedMemlockLimit lim;
    ASSERT_TRUE(lim.valid());
    ASSERT_TRUE(lim.set_bytes(ps * 3));

    auto* addr = static_cast<char*>(
        mmap(nullptr, ps * 3, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, addr);
    ASSERT_EQ(0, mlock(addr, ps * 3));

    errno = 0;
    void* moved = reinterpret_cast<void*>(
        syscall(SYS_mremap, addr + ps, ps, ps, MREMAP_MAYMOVE | MREMAP_DONTUNMAP, 0));
    ASSERT_NE(MAP_FAILED, moved) << "errno=" << errno << " (" << strerror(errno) << ")";

    errno = 0;
    EXPECT_EQ(0, msync(addr, ps, MS_INVALIDATE))
        << "errno=" << errno << " (" << strerror(errno) << ")";

    errno = 0;
    EXPECT_EQ(0, msync(addr + ps, ps, MS_INVALIDATE))
        << "errno=" << errno << " (" << strerror(errno) << ")";

    errno = 0;
    EXPECT_EQ(0, msync(addr + 2 * ps, ps, MS_INVALIDATE))
        << "errno=" << errno << " (" << strerror(errno) << ")";

    errno = 0;
    EXPECT_EQ(-1, msync(moved, ps, MS_INVALIDATE));
    EXPECT_EQ(EBUSY, errno);

    EXPECT_EQ(0, munmap(moved, ps));
    EXPECT_EQ(0, munmap(addr, ps * 3));
}

TEST(Mremap, DontUnmapRejectsUnalignedNewAddress) {
    const size_t ps = PageSize();
    auto* addr = static_cast<char*>(
        mmap(nullptr, ps, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, addr);

    errno = 0;
    void* moved = reinterpret_cast<void*>(
        syscall(SYS_mremap, addr, ps, ps, MREMAP_MAYMOVE | MREMAP_DONTUNMAP, addr + 1));
    EXPECT_EQ(MAP_FAILED, moved);
    EXPECT_EQ(EINVAL, errno);

    addr[0] = 0x5a;
    EXPECT_EQ(0x5a, addr[0]);
    EXPECT_EQ(0, munmap(addr, ps));
}

TEST(Mremap, DontUnmapUsesNewAddressHintWhenAvailable) {
    const size_t ps = PageSize();
    auto* addr = static_cast<char*>(
        mmap(nullptr, ps, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, addr);

    void* hint = mmap(nullptr, ps, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, hint);
    ASSERT_EQ(0, munmap(hint, ps));

    errno = 0;
    void* moved =
        reinterpret_cast<void*>(syscall(SYS_mremap, addr, ps, ps,
                                        MREMAP_MAYMOVE | MREMAP_DONTUNMAP, hint));
    ASSERT_NE(MAP_FAILED, moved) << "errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_EQ(hint, moved);

    EXPECT_EQ(0, munmap(moved, ps));
    EXPECT_EQ(0, munmap(addr, ps));
}

TEST(Mremap, DuplicateOldLenZeroKeepsSourceLock) {
    const size_t ps = PageSize();
    ScopedMemlockLimit lim;
    ASSERT_TRUE(lim.valid());
    ASSERT_TRUE(lim.set_bytes(ps * 4));

    auto* addr = static_cast<char*>(
        mmap(nullptr, ps, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, addr);
    ASSERT_EQ(0, mlock(addr, ps));

    errno = 0;
    void* dup = reinterpret_cast<void*>(syscall(SYS_mremap, addr, 0, ps, MREMAP_MAYMOVE));
    ASSERT_NE(MAP_FAILED, dup) << "errno=" << errno << " (" << strerror(errno) << ")";

    errno = 0;
    EXPECT_EQ(-1, msync(dup, ps, MS_INVALIDATE));
    EXPECT_EQ(EBUSY, errno);

    EXPECT_EQ(0, munlock(dup, ps)) << "errno=" << errno << " (" << strerror(errno) << ")";

    // Linux 6.6 only clears VM_LOCKED on the old VMA for MREMAP_DONTUNMAP.
    // The legacy old_len==0 duplicate path keeps the source VMA locked while
    // also creating a locked duplicate.
    errno = 0;
    EXPECT_EQ(-1, msync(addr, ps, MS_INVALIDATE)) << "errno=" << errno << " (" << strerror(errno)
                                                  << ")";
    EXPECT_EQ(EBUSY, errno);

    EXPECT_EQ(0, munmap(dup, ps));
    EXPECT_EQ(0, munmap(addr, ps));
}

TEST(Madvise, DontNeedKeepsLinuxOrderedSideEffectsBeforeLockedVmaError) {
    const size_t ps = PageSize();
    ScopedMemlockLimit lim;
    ASSERT_TRUE(lim.valid());
    ASSERT_TRUE(lim.set_bytes(ps * 2));

    auto* first = static_cast<char*>(
        mmap(nullptr, ps, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, first);
    memset(first, 0x5a, ps);

    auto* second = static_cast<char*>(
        mmap(first + ps, ps, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0));
    if (second == MAP_FAILED) {
        EXPECT_EQ(0, munmap(first, ps));
        GTEST_SKIP() << "failed to reserve adjacent VMA: errno=" << errno << " ("
                     << strerror(errno) << ")";
    }
    ASSERT_EQ(first + ps, second);
    ASSERT_EQ(0, mlock(second, ps)) << "errno=" << errno << " (" << strerror(errno) << ")";

    errno = 0;
    EXPECT_EQ(-1, madvise(first, ps * 2, MADV_DONTNEED));
    EXPECT_EQ(EINVAL, errno);

    EXPECT_EQ('\0', first[0])
        << "Linux applies MADV_DONTNEED to earlier valid VMAs before returning EINVAL for a later locked VMA";

    EXPECT_EQ(0, munmap(first, ps * 2));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
