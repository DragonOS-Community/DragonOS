#include <gtest/gtest.h>

#include <algorithm>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/shm.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef SYS_mremap
#define SYS_mremap 25
#endif

#ifndef MREMAP_MAYMOVE
#define MREMAP_MAYMOVE 1
#endif

#ifndef MREMAP_FIXED
#define MREMAP_FIXED 2
#endif

#ifndef MREMAP_DONTUNMAP
#define MREMAP_DONTUNMAP 4
#endif

#ifndef MAP_FIXED_NOREPLACE
#define MAP_FIXED_NOREPLACE 0x100000
#endif

#ifndef SHM_EXEC
#define SHM_EXEC 0100000
#endif

#ifndef SHM_STAT
#define SHM_STAT 13
#endif

#ifndef SHM_STAT_ANY
#define SHM_STAT_ANY 15
#endif

#ifndef IPC_INFO
#define IPC_INFO 3
#endif

#ifndef SHM_INFO
#define SHM_INFO 14
#endif

#ifndef SHM_LOCKED
#define SHM_LOCKED 02000
#endif

namespace {

size_t PageSize() {
    const long ps = sysconf(_SC_PAGESIZE);
    return ps > 0 ? static_cast<size_t>(ps) : 4096;
}

size_t CurrentVmSizeBytes() {
    FILE* fp = fopen("/proc/self/status", "r");
    if (fp == nullptr) {
        return 0;
    }

    char line[256];
    size_t kb = 0;
    while (fgets(line, sizeof(line), fp) != nullptr) {
        if (sscanf(line, "VmSize: %zu kB", &kb) == 1) {
            break;
        }
    }
    fclose(fp);
    return kb * 1024;
}

size_t SegmentSize() {
    return PageSize() * 4;
}

uintptr_t MmapMinAddr() {
    return 65536;
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

  private:
    struct rlimit saved_ {};
    bool valid_ = false;
};

class ScopedAddressSpaceLimit {
  public:
    ScopedAddressSpaceLimit() : valid_(getrlimit(RLIMIT_AS, &saved_) == 0) {}

    ~ScopedAddressSpaceLimit() {
        if (valid_) {
            (void)setrlimit(RLIMIT_AS, &saved_);
        }
    }

    bool valid() const {
        return valid_;
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
        return setrlimit(RLIMIT_AS, &lim) == 0;
    }

    bool restore() {
        if (!valid_) {
            return false;
        }
        return setrlimit(RLIMIT_AS, &saved_) == 0;
    }

  private:
    struct rlimit saved_ {};
    bool valid_ = false;
};

key_t UniqueKey() {
    static int seq = 0;
    return static_cast<key_t>(0x53000000 ^ (getpid() << 8) ^ (++seq));
}

class ShmSegment {
  public:
    explicit ShmSegment(size_t size, int flags = IPC_CREAT | 0600) {
        id_ = shmget(IPC_PRIVATE, size, flags);
    }

    ShmSegment(key_t key, size_t size, int flags) {
        id_ = shmget(key, size, flags);
    }

    ~ShmSegment() {
        if (id_ >= 0 && owns_) {
            shmctl(id_, IPC_RMID, nullptr);
        }
    }

    ShmSegment(const ShmSegment&) = delete;
    ShmSegment& operator=(const ShmSegment&) = delete;

    bool valid() const {
        return id_ >= 0;
    }

    int id() const {
        return id_;
    }

    int release() {
        owns_ = false;
        return id_;
    }

  private:
    int id_ = -1;
    bool owns_ = true;
};

int ShmNattch(int shmid) {
    struct shmid_ds ds;
    if (shmctl(shmid, IPC_STAT, &ds) != 0) {
        return -1;
    }
    return static_cast<int>(ds.shm_nattch);
}

void* Attach(int shmid, int flags = 0) {
    void* addr = shmat(shmid, nullptr, flags);
    return addr == reinterpret_cast<void*>(-1) ? MAP_FAILED : addr;
}

void ExpectChildDiesBySignal(int signo, void (*fn)(void*), void* arg) {
    const pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        fn(arg);
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0))
        << "waitpid failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_TRUE(WIFSIGNALED(status)) << "child exited without signal, status=" << status;
    EXPECT_EQ(signo, WTERMSIG(status)) << "unexpected signal, status=" << status;
}

void WriteFirstByte(void* arg) {
    volatile char* p = static_cast<volatile char*>(arg);
    p[0] = 'x';
}

void WaitChildOk(pid_t child) {
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0))
        << "waitpid failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_TRUE(WIFEXITED(status)) << "child did not exit normally, status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status)) << "child failed, status=" << status;
}

void DropPageCache() {
    int fd = open("/proc/sys/vm/drop_caches", O_WRONLY);
    ASSERT_GE(fd, 0) << "open(drop_caches) failed: errno=" << errno << " (" << strerror(errno)
                     << ")";
    const char value[] = "1\n";
    ASSERT_EQ(static_cast<ssize_t>(sizeof(value) - 1), write(fd, value, sizeof(value) - 1))
        << "write(drop_caches) failed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_EQ(0, close(fd));
}

int RunShmExecPermissionScenario() {
    int id = shmget(IPC_PRIVATE, SegmentSize(), IPC_CREAT | 0600);
    if (id < 0) {
        return 1;
    }

    errno = 0;
    void* denied = shmat(id, nullptr, SHM_EXEC);
    if (denied != reinterpret_cast<void*>(-1) || errno != EACCES) {
        if (denied != reinterpret_cast<void*>(-1)) {
            shmdt(denied);
        }
        shmctl(id, IPC_RMID, nullptr);
        return 2;
    }

    void* addr = shmat(id, nullptr, 0);
    if (addr == reinterpret_cast<void*>(-1)) {
        shmctl(id, IPC_RMID, nullptr);
        return 3;
    }
    if (mprotect(addr, SegmentSize(), PROT_READ | PROT_EXEC) != 0) {
        shmdt(addr);
        shmctl(id, IPC_RMID, nullptr);
        return 4;
    }
    shmdt(addr);
    shmctl(id, IPC_RMID, nullptr);

    id = shmget(IPC_PRIVATE, SegmentSize(), IPC_CREAT | 0700);
    if (id < 0) {
        return 5;
    }
    void* exec_addr = shmat(id, nullptr, SHM_EXEC);
    if (exec_addr == reinterpret_cast<void*>(-1)) {
        shmctl(id, IPC_RMID, nullptr);
        return 6;
    }
    shmdt(exec_addr);
    shmctl(id, IPC_RMID, nullptr);
    return 0;
}

int RunMremapFixedLowAddressOrderScenario() {
    const size_t ps = PageSize();
    char* source = static_cast<char*>(
        mmap(nullptr, ps * 2, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    if (source == MAP_FAILED) {
        return 1;
    }
    source[0] = 'a';
    source[ps] = 'b';

    void* low_target = reinterpret_cast<void*>(ps);
    errno = 0;
    void* moved = reinterpret_cast<void*>(
        syscall(SYS_mremap, source, ps * 2, ps, MREMAP_MAYMOVE | MREMAP_FIXED, low_target));
    if (moved != MAP_FAILED || errno != EPERM) {
        if (moved != MAP_FAILED) {
            munmap(moved, ps);
        } else {
            munmap(source, ps * 2);
        }
        return 2;
    }

    if (source[0] != 'a') {
        munmap(source, ps);
        return 3;
    }

    void* tail = mmap(source + ps, ps, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    if (tail != source + ps) {
        munmap(source, ps);
        return errno == EEXIST ? 4 : 5;
    }
    munmap(tail, ps);
    munmap(source, ps);

    char* private_source = static_cast<char*>(
        mmap(nullptr, ps, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    if (private_source == MAP_FAILED) {
        return 6;
    }

    errno = 0;
    moved = reinterpret_cast<void*>(syscall(SYS_mremap, private_source, 0, ps,
                                            MREMAP_MAYMOVE | MREMAP_FIXED, low_target));
    const int saved_errno = errno;
    munmap(private_source, ps);
    if (moved != MAP_FAILED || saved_errno != EINVAL) {
        if (moved != MAP_FAILED) {
            munmap(moved, ps);
        }
        return 7;
    }

    return 0;
}

int RunShmdtReleasesLockedVmScenario() {
    ScopedMemlockLimit lim;
    if (!lim.valid() || !lim.set_bytes(SegmentSize())) {
        return 1;
    }

    ShmSegment shm(SegmentSize());
    if (!shm.valid()) {
        return 2;
    }

    char* addr = static_cast<char*>(Attach(shm.id()));
    if (addr == MAP_FAILED) {
        return 3;
    }
    if (mlock(addr, SegmentSize()) != 0) {
        shmdt(addr);
        return 4;
    }
    if (shmdt(addr) != 0) {
        return 5;
    }

    void* probe = mmap(nullptr, SegmentSize(), PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (probe == MAP_FAILED) {
        return 6;
    }
    const int lock_result = mlock(probe, SegmentSize());
    const int saved_errno = errno;
    munlock(probe, SegmentSize());
    munmap(probe, SegmentSize());
    if (lock_result != 0) {
        return saved_errno == ENOMEM ? 7 : 8;
    }
    return 0;
}

int RunLockedRmidLastDetachReleasesMemlockScenario() {
    ScopedMemlockLimit lim;
    if (!lim.valid() || !lim.set_bytes(SegmentSize())) {
        return 1;
    }

    ShmSegment first(SegmentSize());
    if (!first.valid()) {
        return 2;
    }
    char* addr = static_cast<char*>(Attach(first.id()));
    if (addr == MAP_FAILED) {
        return 3;
    }
    if (shmctl(first.id(), SHM_LOCK, nullptr) != 0) {
        shmdt(addr);
        return 4;
    }
    if (shmctl(first.id(), IPC_RMID, nullptr) != 0) {
        shmdt(addr);
        return 5;
    }
    if (shmdt(addr) != 0) {
        return 6;
    }

    ShmSegment second(SegmentSize());
    if (!second.valid()) {
        return 7;
    }
    if (shmctl(second.id(), SHM_LOCK, nullptr) != 0) {
        return errno == ENOMEM ? 8 : 9;
    }
    if (shmctl(second.id(), SHM_UNLOCK, nullptr) != 0) {
        return 10;
    }
    return 0;
}

constexpr int kIpcIdIndexMask = (1 << 15) - 1;

}  // namespace

TEST(SysvShmSemantics, ReusedLowIndexRejectsStaleFullId) {
    ShmSegment first(SegmentSize());
    ASSERT_TRUE(first.valid()) << "first shmget failed: errno=" << errno << " ("
                               << strerror(errno) << ")";
    const int stale_id = first.release();
    const int stale_idx = stale_id & kIpcIdIndexMask;
    ASSERT_EQ(0, shmctl(stale_id, IPC_RMID, nullptr))
        << "IPC_RMID failed: errno=" << errno << " (" << strerror(errno) << ")";

    ShmSegment second(SegmentSize());
    ASSERT_TRUE(second.valid()) << "second shmget failed: errno=" << errno << " ("
                                << strerror(errno) << ")";
    const int current_id = second.id();
    const int current_idx = current_id & kIpcIdIndexMask;
    if (current_idx != stale_idx || current_id == stale_id) {
        GTEST_SKIP() << "allocator did not immediately reuse the low IPC index";
    }

    struct shmid_ds ds;
    errno = 0;
    EXPECT_EQ(-1, shmctl(stale_id, IPC_STAT, &ds));
    EXPECT_EQ(EINVAL, errno);

    errno = 0;
    void* stale_attach = shmat(stale_id, nullptr, 0);
    EXPECT_EQ(reinterpret_cast<void*>(-1), stale_attach);
    EXPECT_EQ(EINVAL, errno);

    errno = 0;
    EXPECT_EQ(current_id, shmctl(current_idx, SHM_STAT, &ds));
    EXPECT_EQ(0, errno);

    errno = 0;
    EXPECT_EQ(current_id, shmctl(current_idx, SHM_STAT_ANY, &ds));
    EXPECT_EQ(0, errno);
}

TEST(SysvShmSemantics, ShmInfoReturnsCurrentMaxIndex) {
    ShmSegment first(SegmentSize());
    ASSERT_TRUE(first.valid()) << "first shmget failed: errno=" << errno << " ("
                               << strerror(errno) << ")";
    ShmSegment second(SegmentSize());
    ASSERT_TRUE(second.valid()) << "second shmget failed: errno=" << errno << " ("
                                << strerror(errno) << ")";

    const int expected_min_max_idx =
        std::max(first.id() & kIpcIdIndexMask, second.id() & kIpcIdIndexMask);
    struct shm_info info;
    memset(&info, 0, sizeof(info));
    errno = 0;
    const int max_idx =
        shmctl(0, SHM_INFO, reinterpret_cast<struct shmid_ds*>(&info));
    ASSERT_GE(max_idx, 0) << "SHM_INFO failed: errno=" << errno << " (" << strerror(errno)
                          << ")";
    EXPECT_GE(max_idx, expected_min_max_idx);
    EXPECT_GE(info.used_ids, 2);

    char* addr = static_cast<char*>(Attach(first.id()));
    ASSERT_NE(MAP_FAILED, addr);
    addr[0] = 'r';
    memset(&info, 0, sizeof(info));
    ASSERT_GE(shmctl(0, SHM_INFO, reinterpret_cast<struct shmid_ds*>(&info)), 0)
        << "SHM_INFO after fault failed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_GE(info.shm_rss, 1UL);
    EXPECT_EQ(0, shmdt(addr));
}

TEST(SysvShmSemantics, ShmctlRejectsNegativeIdBeforeInfoCommands) {
    struct shminfo ipc_info;
    memset(&ipc_info, 0, sizeof(ipc_info));
    errno = 0;
    EXPECT_EQ(-1, shmctl(-1, IPC_INFO, reinterpret_cast<struct shmid_ds*>(&ipc_info)));
    EXPECT_EQ(EINVAL, errno);

    struct shm_info info;
    memset(&info, 0, sizeof(info));
    errno = 0;
    EXPECT_EQ(-1, shmctl(-1, SHM_INFO, reinterpret_cast<struct shmid_ds*>(&info)));
    EXPECT_EQ(EINVAL, errno);
}

TEST(SysvShmSemantics, NegativeKeyRoundTripsThroughIpcStat) {
    const key_t key = static_cast<key_t>(-1);
    const int id = shmget(key, SegmentSize(), IPC_CREAT | IPC_EXCL | 0600);
    if (id < 0 && errno == EEXIST) {
        GTEST_SKIP() << "negative key already exists";
    }
    ASSERT_GE(id, 0) << "shmget negative key failed: errno=" << errno << " (" << strerror(errno)
                     << ")";

    struct shmid_ds ds;
    ASSERT_EQ(0, shmctl(id, IPC_STAT, &ds));
    EXPECT_EQ(key, ds.shm_perm.__key);
    EXPECT_EQ(0, shmctl(id, IPC_RMID, nullptr));
}

TEST(SysvShmSemantics, ShmLockUnlockUpdatesMode) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid()) << "shmget failed: errno=" << errno << " (" << strerror(errno)
                             << ")";

    struct shmid_ds ds;
    ASSERT_EQ(0, shmctl(shm.id(), IPC_STAT, &ds));
    EXPECT_EQ(0u, ds.shm_perm.mode & SHM_LOCKED);
    const time_t original_ctime = ds.shm_ctime;
    sleep(1);

    ASSERT_EQ(0, shmctl(shm.id(), SHM_LOCK, nullptr))
        << "SHM_LOCK failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_EQ(0, shmctl(shm.id(), IPC_STAT, &ds));
    EXPECT_NE(0u, ds.shm_perm.mode & SHM_LOCKED);
    EXPECT_EQ(original_ctime, ds.shm_ctime);

    EXPECT_EQ(0, shmctl(shm.id(), SHM_LOCK, nullptr))
        << "repeat SHM_LOCK failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_EQ(0, shmctl(shm.id(), IPC_STAT, &ds));
    EXPECT_NE(0u, ds.shm_perm.mode & SHM_LOCKED);
    EXPECT_EQ(original_ctime, ds.shm_ctime);

    ASSERT_EQ(0, shmctl(shm.id(), SHM_UNLOCK, nullptr))
        << "SHM_UNLOCK failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_EQ(0, shmctl(shm.id(), IPC_STAT, &ds));
    EXPECT_EQ(0u, ds.shm_perm.mode & SHM_LOCKED);
    EXPECT_EQ(original_ctime, ds.shm_ctime);

    EXPECT_EQ(0, shmctl(shm.id(), SHM_UNLOCK, nullptr))
        << "repeat SHM_UNLOCK failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_EQ(0, shmctl(shm.id(), IPC_STAT, &ds));
    EXPECT_EQ(original_ctime, ds.shm_ctime);
}

TEST(SysvShmSemantics, LockedRmidLastDetachReleasesMemlockAccounting) {
    if (geteuid() == 0) {
        const pid_t child = fork();
        ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
        if (child == 0) {
            if (setgid(1000) != 0 || setuid(1000) != 0) {
                _exit(10);
            }
            _exit(RunLockedRmidLastDetachReleasesMemlockScenario());
        }
        WaitChildOk(child);
    } else {
        EXPECT_EQ(0, RunLockedRmidLastDetachReleasesMemlockScenario());
    }
}

TEST(SysvShmSemantics, ShmNattchTracksAttachDetach) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid()) << "shmget failed: errno=" << errno << " (" << strerror(errno)
                             << ")";
    EXPECT_EQ(0, ShmNattch(shm.id()));

    void* addr1 = Attach(shm.id());
    ASSERT_NE(MAP_FAILED, addr1) << "shmat failed: errno=" << errno << " (" << strerror(errno)
                                 << ")";
    EXPECT_EQ(1, ShmNattch(shm.id()));

    void* addr2 = Attach(shm.id());
    ASSERT_NE(MAP_FAILED, addr2) << "second shmat failed: errno=" << errno << " ("
                                 << strerror(errno) << ")";
    EXPECT_EQ(2, ShmNattch(shm.id()));

    EXPECT_EQ(0, shmdt(addr1)) << "shmdt(addr1) failed: errno=" << errno << " ("
                               << strerror(errno) << ")";
    EXPECT_EQ(1, ShmNattch(shm.id()));

    EXPECT_EQ(0, shmdt(addr2)) << "shmdt(addr2) failed: errno=" << errno << " ("
                               << strerror(errno) << ")";
    EXPECT_EQ(0, ShmNattch(shm.id()));
}

TEST(SysvShmSemantics, DetachedSegmentPersists) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    addr[0] = 'D';
    ASSERT_EQ(0, shmdt(addr));

    addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    EXPECT_EQ('D', addr[0]);
    EXPECT_EQ(0, shmdt(addr));
}

TEST(SysvShmSemantics, ShmdtRejectsNonShmMapping) {
    const size_t ps = PageSize();
    char* mapping = static_cast<char*>(
        mmap(nullptr, ps, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, mapping);

    mapping[0] = 'a';
    errno = 0;
    EXPECT_EQ(-1, shmdt(mapping));
    EXPECT_EQ(EINVAL, errno);
    mapping[0] = 'b';

    EXPECT_EQ(0, munmap(mapping, ps));
}

TEST(SysvShmSemantics, ReadonlyAttachRejectsWrite) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id(), SHM_RDONLY));
    ASSERT_NE(MAP_FAILED, addr);
    static_cast<void>(addr[0]);

    ExpectChildDiesBySignal(SIGSEGV, WriteFirstByte, addr);
    EXPECT_EQ(0, shmdt(addr));
}

TEST(SysvShmSemantics, ReadonlyAttachMprotectWriteDenied) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    void* addr = Attach(shm.id(), SHM_RDONLY);
    ASSERT_NE(MAP_FAILED, addr);

    errno = 0;
    EXPECT_EQ(-1, mprotect(addr, SegmentSize(), PROT_READ | PROT_WRITE));
    EXPECT_EQ(EACCES, errno);

    EXPECT_EQ(0, shmdt(addr));
}

TEST(SysvShmSemantics, MprotectZeroLengthSucceeds) {
    void* mapping = mmap(nullptr, PageSize(), PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping);

    errno = 0;
    EXPECT_EQ(0, mprotect(mapping, 0, PROT_NONE));
    EXPECT_EQ(0, errno);

    EXPECT_EQ(0, munmap(mapping, PageSize()));
}

TEST(SysvShmSemantics, MprotectAndMadviseReportUnmappedHoles) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    const size_t ps = PageSize();
    addr[0] = 'h';
    addr[ps * 2] = 'i';

    ASSERT_EQ(0, munmap(addr + ps, ps));

    errno = 0;
    EXPECT_EQ(-1, mprotect(addr, SegmentSize(), PROT_READ));
    EXPECT_EQ(ENOMEM, errno);

    errno = 0;
    EXPECT_EQ(-1, madvise(addr, SegmentSize(), MADV_RANDOM));
    EXPECT_EQ(ENOMEM, errno);

    EXPECT_EQ('h', addr[0]);
    EXPECT_EQ('i', addr[ps * 2]);
    EXPECT_EQ(0, shmdt(addr));
}

TEST(SysvShmSemantics, PartialUnmapThenShmdt) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    addr[0] = 'p';
    addr[SegmentSize() - 1] = 'q';

    const size_t ps = PageSize();
    ASSERT_EQ(0, munmap(addr + ps, ps * 2));
    EXPECT_EQ(0, shmdt(addr));
}

TEST(SysvShmSemantics, PartialUnmapNattchTracksFragments) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    EXPECT_EQ(1, ShmNattch(shm.id()));

    const size_t ps = PageSize();
    ASSERT_EQ(0, munmap(addr + ps, ps * 2));
    EXPECT_EQ(2, ShmNattch(shm.id()));

    ASSERT_EQ(0, shmctl(shm.id(), IPC_RMID, nullptr));
    addr[0] = 'x';
    addr[SegmentSize() - 1] = 'y';
    EXPECT_EQ(0, shmdt(addr));

    shm.release();
}

TEST(SysvShmSemantics, RepeatedRmidWhileAttachedDoesNotDestroy) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr1 = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr1);
    char* addr2 = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr2);

    const int id = shm.release();
    for (int i = 0; i < 6; ++i) {
        ASSERT_EQ(0, shmctl(id, IPC_RMID, nullptr));
    }

    addr1[0] = 'a';
    addr2[0] = 'b';
    EXPECT_EQ(0, shmdt(addr1));
    addr2[0] = 'c';
    EXPECT_EQ(0, shmdt(addr2));
}

TEST(SysvShmSemantics, AllowsAttachToRemovedSegmentWithRefs) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr1 = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr1);

    const int id = shm.release();
    ASSERT_EQ(0, shmctl(id, IPC_RMID, nullptr));

    char* addr2 = static_cast<char*>(Attach(id));
    ASSERT_NE(MAP_FAILED, addr2) << "attach to removed-but-referenced segment failed: errno="
                                 << errno << " (" << strerror(errno) << ")";
    addr1[0] = 'r';
    EXPECT_EQ('r', addr2[0]);

    EXPECT_EQ(0, shmdt(addr1));
    EXPECT_EQ(0, shmdt(addr2));
}

TEST(SysvShmSemantics, RemovedSegmentsAreNotDiscoverable) {
    const key_t key = UniqueKey();
    ShmSegment shm(key, SegmentSize(), IPC_CREAT | IPC_EXCL | 0600);
    ASSERT_TRUE(shm.valid()) << "shmget key failed: errno=" << errno << " (" << strerror(errno)
                             << ")";

    void* addr = Attach(shm.id());
    ASSERT_NE(MAP_FAILED, addr);
    ASSERT_EQ(0, shmctl(shm.id(), IPC_RMID, nullptr));
    shm.release();

    errno = 0;
    EXPECT_EQ(-1, shmget(key, SegmentSize(), 0600));
    EXPECT_EQ(ENOENT, errno);

    EXPECT_EQ(0, shmdt(addr));
}

TEST(SysvShmSemantics, ExistingKeyAndControlsHonorIpcPermissions) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root to drop credentials";
    }

    const key_t key = UniqueKey();
    ShmSegment shm(key, SegmentSize(), IPC_CREAT | IPC_EXCL | 0600);
    ASSERT_TRUE(shm.valid()) << "shmget key failed: errno=" << errno << " (" << strerror(errno)
                             << ")";

    const pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
    if (child == 0) {
        if (setgid(1000) != 0 || setuid(1000) != 0) {
            _exit(10);
        }

        errno = 0;
        if (shmget(key, 0, 0600) != -1 || errno != EACCES) {
            _exit(20);
        }

        errno = 0;
        void* addr = shmat(shm.id(), nullptr, SHM_RDONLY);
        if (addr != reinterpret_cast<void*>(-1) || errno != EACCES) {
            if (addr != reinterpret_cast<void*>(-1)) {
                shmdt(addr);
            }
            _exit(21);
        }

        errno = 0;
        if (shmctl(shm.id(), IPC_RMID, nullptr) != -1 || errno != EPERM) {
            _exit(22);
        }
        _exit(0);
    }
    WaitChildOk(child);
}

TEST(SysvShmSemantics, ShmExecRequiresExecutePermissionAtAttach) {
    if (geteuid() == 0) {
        const pid_t child = fork();
        ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
        if (child == 0) {
            if (setgid(1000) != 0 || setuid(1000) != 0) {
                _exit(10);
            }
            _exit(RunShmExecPermissionScenario());
        }
        WaitChildOk(child);
    } else {
        EXPECT_EQ(0, RunShmExecPermissionScenario());
    }
}

TEST(SysvShmSemantics, ShmatIgnoresNonAttachFlagBitsLikeLinux) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* base = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, base);
    base[0] = 'q';

    constexpr int kUnknownAttachFlag = 040000000;
    const int extra_flags[] = {
        IPC_CREAT,
        IPC_EXCL,
        IPC_CREAT | IPC_EXCL,
        kUnknownAttachFlag,
        SHM_RDONLY | IPC_CREAT | kUnknownAttachFlag,
    };

    for (int flags : extra_flags) {
        errno = 0;
        char* addr = static_cast<char*>(shmat(shm.id(), nullptr, flags));
        ASSERT_NE(reinterpret_cast<void*>(-1), addr)
            << "shmat unexpectedly rejected flags 0" << std::oct << flags << std::dec
            << ": errno=" << errno << " (" << strerror(errno) << ")";
        EXPECT_EQ('q', addr[0]);
        EXPECT_EQ(0, shmdt(addr));
    }

    EXPECT_EQ(0, shmdt(base));
}

TEST(SysvShmSemantics, FixedAttachWithoutRemapDoesNotReplaceExistingMapping) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());
    EXPECT_EQ(0, ShmNattch(shm.id()));

    char* target = static_cast<char*>(
        mmap(nullptr, SegmentSize(), PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, target);
    target[0] = 'n';

    errno = 0;
    void* denied = shmat(shm.id(), target, 0);
    EXPECT_EQ(reinterpret_cast<void*>(-1), denied);
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ('n', target[0]);
    EXPECT_EQ(0, ShmNattch(shm.id()));

    EXPECT_EQ(0, munmap(target, SegmentSize()));
}

TEST(SysvShmSemantics, ShmatLowAddressConflictReturnsEinvalBeforeMmapMin) {
    const size_t ps = PageSize();
    void* low = reinterpret_cast<void*>(ps);
    bool own_guard = false;
    char* guard = static_cast<char*>(mmap(low, ps, PROT_READ | PROT_WRITE,
                                          MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1,
                                          0));
    if (guard == MAP_FAILED && errno == EPERM) {
        GTEST_SKIP() << "caller lacks CAP_SYS_RAWIO for mmap_min_addr";
    }
    if (guard == MAP_FAILED && errno == EEXIST) {
        GTEST_SKIP() << "low address already unavailable in this process";
    }
    ASSERT_EQ(low, guard) << "low guard mmap failed: errno=" << errno << " (" << strerror(errno)
                          << ")";
    own_guard = true;

    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    errno = 0;
    void* denied = shmat(shm.id(), low, 0);
    EXPECT_EQ(reinterpret_cast<void*>(-1), denied);
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(0, ShmNattch(shm.id()));

    if (own_guard) {
        EXPECT_EQ(0, munmap(guard, ps));
    }
}

TEST(SysvShmSemantics, FailedLockedMapFixedReplacementKeepsAttachConsistent) {
    ScopedMemlockLimit lim;
    ASSERT_TRUE(lim.valid());
    if (!lim.set_bytes(0)) {
        GTEST_SKIP() << "cannot lower RLIMIT_MEMLOCK";
    }

    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    ASSERT_EQ(1, ShmNattch(shm.id()));
    addr[0] = 'a';

    errno = 0;
    void* replaced = mmap(addr, SegmentSize(), PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED | MAP_LOCKED, -1, 0);
    if (replaced == MAP_FAILED) {
        ASSERT_TRUE(errno == EPERM || errno == EAGAIN)
            << "unexpected errno=" << errno << " (" << strerror(errno) << ")";
        EXPECT_EQ(1, ShmNattch(shm.id()));
        EXPECT_EQ('a', addr[0]);
        EXPECT_EQ(0, shmdt(addr));
        EXPECT_EQ(0, ShmNattch(shm.id()));
        return;
    }

    ASSERT_EQ(addr, replaced);
    EXPECT_EQ(0, ShmNattch(shm.id()));
    EXPECT_EQ(0, munmap(replaced, SegmentSize()));
}

TEST(SysvShmSemantics, FailedLockedFileMapFixedReplacementKeepsAttachConsistent) {
    ScopedMemlockLimit lim;
    ASSERT_TRUE(lim.valid());
    if (!lim.set_bytes(0)) {
        GTEST_SKIP() << "cannot lower RLIMIT_MEMLOCK";
    }

    char tmpl[] = "/tmp/dunitest_shm_file_XXXXXX";
    int fd = mkstemp(tmpl);
    ASSERT_GE(fd, 0) << "mkstemp failed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_EQ(0, unlink(tmpl));
    ASSERT_EQ(0, ftruncate(fd, static_cast<off_t>(SegmentSize())))
        << "ftruncate failed: errno=" << errno << " (" << strerror(errno) << ")";

    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    ASSERT_EQ(1, ShmNattch(shm.id()));
    addr[0] = 'f';

    errno = 0;
    void* replaced =
        mmap(addr, SegmentSize(), PROT_READ, MAP_PRIVATE | MAP_FIXED | MAP_LOCKED, fd, 0);
    if (replaced == MAP_FAILED) {
        ASSERT_TRUE(errno == EPERM || errno == EAGAIN)
            << "unexpected errno=" << errno << " (" << strerror(errno) << ")";
        EXPECT_EQ(1, ShmNattch(shm.id()));
        EXPECT_EQ('f', addr[0]);
        EXPECT_EQ(0, shmdt(addr));
        EXPECT_EQ(0, ShmNattch(shm.id()));
        EXPECT_EQ(0, close(fd));
        return;
    }

    ASSERT_EQ(addr, replaced);
    EXPECT_EQ(0, ShmNattch(shm.id()));
    EXPECT_EQ(0, munmap(replaced, SegmentSize()));
    EXPECT_EQ(0, close(fd));
}

TEST(SysvShmSemantics, MapFixedReplacementUsesNetRlimitAsGrowth) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    ASSERT_EQ(1, ShmNattch(shm.id()));
    addr[0] = 'r';

    ScopedAddressSpaceLimit lim;
    ASSERT_TRUE(lim.valid());
    const size_t current_vm = CurrentVmSizeBytes();
    if (current_vm == 0) {
        GTEST_SKIP() << "cannot read VmSize from /proc/self/status";
    }
    if (!lim.set_bytes(current_vm)) {
        GTEST_SKIP() << "cannot lower RLIMIT_AS";
    }

    errno = 0;
    void* replaced = mmap(addr, SegmentSize(), PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    const int saved_errno = errno;
    ASSERT_TRUE(lim.restore()) << "restore RLIMIT_AS failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    ASSERT_EQ(addr, replaced) << "MAP_FIXED net-zero replacement failed: errno=" << saved_errno
                              << " (" << strerror(saved_errno) << ")";
    EXPECT_EQ(0, ShmNattch(shm.id()));
    EXPECT_EQ(0, munmap(replaced, SegmentSize()));
}

TEST(SysvShmSemantics, FailedRlimitAsAnonMapFixedKeepsAttachConsistent) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    ASSERT_EQ(1, ShmNattch(shm.id()));
    addr[0] = 'a';

    ScopedAddressSpaceLimit lim;
    ASSERT_TRUE(lim.valid());
    if (!lim.set_bytes(PageSize())) {
        GTEST_SKIP() << "cannot lower RLIMIT_AS";
    }

    errno = 0;
    void* replaced = mmap(addr, SegmentSize() * 2, PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    const int saved_errno = errno;
    ASSERT_TRUE(lim.restore()) << "restore RLIMIT_AS failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    ASSERT_EQ(MAP_FAILED, replaced);
    EXPECT_EQ(ENOMEM, saved_errno);
    EXPECT_EQ(1, ShmNattch(shm.id()));
    EXPECT_EQ('a', addr[0]);
    EXPECT_EQ(0, shmdt(addr));
    EXPECT_EQ(0, ShmNattch(shm.id()));
}

TEST(SysvShmSemantics, FailedRlimitAsFileMapFixedKeepsAttachConsistent) {
    char tmpl[] = "/tmp/dunitest_shm_rlimit_as_XXXXXX";
    int fd = mkstemp(tmpl);
    ASSERT_GE(fd, 0) << "mkstemp failed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_EQ(0, unlink(tmpl));
    ASSERT_EQ(0, ftruncate(fd, static_cast<off_t>(SegmentSize() * 2)))
        << "ftruncate failed: errno=" << errno << " (" << strerror(errno) << ")";

    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    ASSERT_EQ(1, ShmNattch(shm.id()));
    addr[0] = 'f';

    ScopedAddressSpaceLimit lim;
    ASSERT_TRUE(lim.valid());
    if (!lim.set_bytes(PageSize())) {
        EXPECT_EQ(0, close(fd));
        GTEST_SKIP() << "cannot lower RLIMIT_AS";
    }

    errno = 0;
    void* replaced =
        mmap(addr, SegmentSize() * 2, PROT_READ, MAP_PRIVATE | MAP_FIXED, fd, 0);
    const int saved_errno = errno;
    ASSERT_TRUE(lim.restore()) << "restore RLIMIT_AS failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    ASSERT_EQ(MAP_FAILED, replaced);
    EXPECT_EQ(ENOMEM, saved_errno);
    EXPECT_EQ(1, ShmNattch(shm.id()));
    EXPECT_EQ('f', addr[0]);
    EXPECT_EQ(0, shmdt(addr));
    EXPECT_EQ(0, ShmNattch(shm.id()));
    EXPECT_EQ(0, close(fd));
}

TEST(SysvShmSemantics, ExplicitLowAttachIsNeverRoundedToMmapMin) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    void* requested = reinterpret_cast<void*>(PageSize());
    errno = 0;
    void* attached = shmat(shm.id(), requested, 0);
    if (attached != reinterpret_cast<void*>(-1)) {
        EXPECT_EQ(requested, attached)
            << "explicit shmat address was silently rewritten";
        EXPECT_EQ(0, shmdt(attached));
    } else {
        EXPECT_NE(0, errno);
    }
    EXPECT_NE(reinterpret_cast<void*>(MmapMinAddr()), attached);
}

TEST(SysvShmSemantics, LowAddressRemapDoesNotOverwriteMmapMinGuard) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* guard = static_cast<char*>(mmap(reinterpret_cast<void*>(MmapMinAddr()), PageSize(),
                                          PROT_READ | PROT_WRITE,
                                          MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1,
                                          0));
    if (guard == MAP_FAILED) {
        GTEST_SKIP() << "mmap_min guard unavailable: errno=" << errno << " (" << strerror(errno)
                     << ")";
    }
    guard[0] = 'g';

    void* requested = reinterpret_cast<void*>(PageSize());
    errno = 0;
    void* attached = shmat(shm.id(), requested, SHM_REMAP);
    if (attached != reinterpret_cast<void*>(-1)) {
        EXPECT_EQ(requested, attached)
            << "SHM_REMAP low explicit address was silently rewritten";
        EXPECT_EQ(0, shmdt(attached));
    } else {
        EXPECT_NE(0, errno);
    }
    EXPECT_EQ('g', guard[0]) << "SHM_REMAP overwrote the rounded mmap_min address";
    EXPECT_EQ(0, munmap(guard, PageSize()));
}

TEST(SysvShmSemantics, CapSysRawioAllowsMapFixedNull) {
    const size_t ps = PageSize();
    errno = 0;
    void* mapped = mmap(nullptr, ps, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    if (mapped == MAP_FAILED && errno == EPERM) {
        GTEST_SKIP() << "caller lacks CAP_SYS_RAWIO for mmap_min_addr";
    }
    ASSERT_EQ(nullptr, mapped) << "MAP_FIXED null mmap failed: errno=" << errno << " ("
                               << strerror(errno) << ")";
    static_cast<char*>(mapped)[0] = '0';
    EXPECT_EQ('0', static_cast<char*>(mapped)[0]);
    EXPECT_EQ(0, munmap(mapped, ps));
}

TEST(SysvShmSemantics, CapSysRawioAllowsMremapFixedNull) {
    const size_t ps = PageSize();
    char* source = static_cast<char*>(
        mmap(nullptr, ps, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, source);
    source[0] = 'r';

    errno = 0;
    void* moved = reinterpret_cast<void*>(
        syscall(SYS_mremap, source, ps, ps, MREMAP_MAYMOVE | MREMAP_FIXED, nullptr));
    if (moved == MAP_FAILED && errno == EPERM) {
        EXPECT_EQ(0, munmap(source, ps));
        GTEST_SKIP() << "caller lacks CAP_SYS_RAWIO for mmap_min_addr";
    }
    ASSERT_EQ(nullptr, moved) << "MREMAP_FIXED null failed: errno=" << errno << " ("
                              << strerror(errno) << ")";
    EXPECT_EQ('r', static_cast<char*>(moved)[0]);
    EXPECT_EQ(0, munmap(moved, ps));
}

TEST(SysvShmSemantics, MremapFixedLowAddressChecksAfterLinuxSideEffects) {
    if (geteuid() == 0) {
        const pid_t child = fork();
        ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
        if (child == 0) {
            if (setgid(1000) != 0 || setuid(1000) != 0) {
                _exit(10);
            }
            _exit(RunMremapFixedLowAddressOrderScenario());
        }
        WaitChildOk(child);
    } else {
        EXPECT_EQ(0, RunMremapFixedLowAddressOrderScenario());
    }
}

TEST(SysvShmSemantics, ShmRemapReplacesExactHighAddress) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* target = static_cast<char*>(
        mmap(nullptr, SegmentSize(), PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, target);
    target[0] = 'r';

    void* attached = shmat(shm.id(), target, SHM_REMAP);
    ASSERT_NE(reinterpret_cast<void*>(-1), attached)
        << "SHM_REMAP failed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_EQ(target, attached);
    static_cast<char*>(attached)[0] = 's';
    EXPECT_EQ('s', static_cast<char*>(attached)[0]);
    EXPECT_EQ(0, shmdt(attached));
}

TEST(SysvShmSemantics, ShmRemapNullFailsWithoutAttach) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());
    EXPECT_EQ(0, ShmNattch(shm.id()));

    errno = 0;
    void* attached = shmat(shm.id(), nullptr, SHM_REMAP);
    EXPECT_EQ(reinterpret_cast<void*>(-1), attached);
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(0, ShmNattch(shm.id()));
}

TEST(SysvShmSemantics, UnalignedAttachRequiresShmRnd) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* reservation = static_cast<char*>(
        mmap(nullptr, SegmentSize() * 2, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, reservation);
    ASSERT_EQ(0, munmap(reservation, SegmentSize() * 2));

    char* unaligned = reservation + PageSize() / 2;
    errno = 0;
    void* denied = shmat(shm.id(), unaligned, 0);
    EXPECT_EQ(reinterpret_cast<void*>(-1), denied);
    EXPECT_EQ(EINVAL, errno);

    void* rounded = shmat(shm.id(), unaligned, SHM_RND);
    ASSERT_NE(reinterpret_cast<void*>(-1), rounded)
        << "SHM_RND attach failed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_EQ(reservation, rounded);
    EXPECT_EQ(0, shmdt(rounded));
}

TEST(SysvShmSemantics, MultipleDetachFails) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    void* addr = Attach(shm.id());
    ASSERT_NE(MAP_FAILED, addr);
    ASSERT_EQ(0, shmdt(addr));

    errno = 0;
    EXPECT_EQ(-1, shmdt(addr));
    EXPECT_EQ(EINVAL, errno);
}

TEST(SysvShmSemantics, SegmentsSizeFixedOnCreation) {
    const key_t key = UniqueKey();
    ShmSegment shm(key, SegmentSize(), IPC_CREAT | IPC_EXCL | 0600);
    ASSERT_TRUE(shm.valid());

    const int same = shmget(key, SegmentSize() / 2, 0600);
    ASSERT_EQ(shm.id(), same);

    errno = 0;
    EXPECT_EQ(-1, shmget(key, SegmentSize() * 2, 0600));
    EXPECT_EQ(EINVAL, errno);

    char* addr = static_cast<char*>(Attach(same));
    ASSERT_NE(MAP_FAILED, addr);
    addr[SegmentSize() - 1] = 's';
    EXPECT_EQ(0, shmdt(addr));
}

TEST(SysvShmSemantics, TwoAttachSplitDetachIsolation) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr1 = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr1);
    char* addr2 = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr2);

    const size_t ps = PageSize();
    ASSERT_EQ(0, mprotect(addr1 + ps, ps, PROT_READ));
    ASSERT_EQ(0, munmap(addr1 + ps * 2, ps));
    ASSERT_EQ(0, shmdt(addr1));

    addr2[0] = 'i';
    addr2[SegmentSize() - 1] = 'j';
    EXPECT_EQ(0, shmdt(addr2));
}

TEST(SysvShmSemantics, MlockSplitNattchTracksFragments) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    EXPECT_EQ(1, ShmNattch(shm.id()));

    const size_t ps = PageSize();
    ASSERT_EQ(0, mlock(addr + ps, ps));
    EXPECT_EQ(3, ShmNattch(shm.id()));

    EXPECT_EQ(0, shmdt(addr));
    EXPECT_EQ(0, ShmNattch(shm.id()));
}

TEST(SysvShmSemantics, RepeatedMlockDoesNotResplitSysvShmVma) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    EXPECT_EQ(1, ShmNattch(shm.id()));

    const size_t ps = PageSize();
    ASSERT_EQ(0, mlock(addr + ps, ps));
    EXPECT_EQ(3, ShmNattch(shm.id()));

    ASSERT_EQ(0, mlock(addr + ps, ps));
    EXPECT_EQ(3, ShmNattch(shm.id()));

    EXPECT_EQ(0, shmdt(addr));
    EXPECT_EQ(0, ShmNattch(shm.id()));
}

TEST(SysvShmSemantics, ShmdtReleasesLockedVmAccounting) {
    if (geteuid() == 0) {
        const pid_t child = fork();
        ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
        if (child == 0) {
            if (setgid(1000) != 0 || setuid(1000) != 0) {
                _exit(10);
            }
            _exit(RunShmdtReleasesLockedVmScenario());
        }
        WaitChildOk(child);
    } else {
        EXPECT_EQ(0, RunShmdtReleasesLockedVmScenario());
    }
}

TEST(SysvShmSemantics, MadviseSplitNattchTracksFragments) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    EXPECT_EQ(1, ShmNattch(shm.id()));

    const size_t ps = PageSize();
    ASSERT_EQ(0, madvise(addr + ps, ps, MADV_RANDOM));
    EXPECT_EQ(3, ShmNattch(shm.id()));

    EXPECT_EQ(0, shmdt(addr));
    EXPECT_EQ(0, ShmNattch(shm.id()));
}

TEST(SysvShmSemantics, RepeatedMadviseDoesNotResplitSysvShmVma) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    EXPECT_EQ(1, ShmNattch(shm.id()));

    const size_t ps = PageSize();
    ASSERT_EQ(0, madvise(addr + ps, ps, MADV_RANDOM));
    EXPECT_EQ(3, ShmNattch(shm.id()));

    ASSERT_EQ(0, madvise(addr + ps, ps, MADV_RANDOM));
    EXPECT_EQ(3, ShmNattch(shm.id()));

    EXPECT_EQ(0, shmdt(addr));
    EXPECT_EQ(0, ShmNattch(shm.id()));
}

TEST(SysvShmSemantics, MadviseDontneedDoesNotSplitSysvShmVma) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    EXPECT_EQ(1, ShmNattch(shm.id()));

    const size_t ps = PageSize();
    addr[0] = 'a';
    addr[ps] = 'b';
    addr[ps * 2] = 'c';

    ASSERT_EQ(0, madvise(addr + ps, ps, MADV_DONTNEED))
        << "MADV_DONTNEED failed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_EQ(1, ShmNattch(shm.id()))
        << "MADV_DONTNEED must zap pages without splitting the SysV SHM VMA";

    EXPECT_EQ(0, shmdt(addr));
    EXPECT_EQ(0, ShmNattch(shm.id()));
}

TEST(SysvShmSemantics, RepeatedMprotectDoesNotResplitSysvShmVma) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    EXPECT_EQ(1, ShmNattch(shm.id()));

    const size_t ps = PageSize();
    ASSERT_EQ(0, mprotect(addr + ps, ps, PROT_READ));
    EXPECT_EQ(3, ShmNattch(shm.id()));

    ASSERT_EQ(0, mprotect(addr + ps, ps, PROT_READ));
    EXPECT_EQ(3, ShmNattch(shm.id()));

    EXPECT_EQ(0, shmdt(addr));
    EXPECT_EQ(0, ShmNattch(shm.id()));
}

TEST(SysvShmSemantics, SysvShmMremapMoveNetNattch) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    addr[0] = 'm';
    EXPECT_EQ(1, ShmNattch(shm.id()));

    void* target = mmap(nullptr, SegmentSize(), PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, target);

    void* moved = reinterpret_cast<void*>(
        syscall(SYS_mremap, addr, SegmentSize(), SegmentSize(), MREMAP_MAYMOVE | MREMAP_FIXED,
                target));
    ASSERT_NE(MAP_FAILED, moved) << "mremap failed: errno=" << errno << " (" << strerror(errno)
                                 << ")";
    EXPECT_EQ(target, moved);
    EXPECT_EQ('m', static_cast<char*>(moved)[0]);
    EXPECT_EQ(1, ShmNattch(shm.id()));
    ExpectChildDiesBySignal(SIGSEGV, WriteFirstByte, addr);

    EXPECT_EQ(0, shmdt(moved));
}

TEST(SysvShmSemantics, MremapFixedWrapTargetFailsWithoutChangingSource) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    addr[0] = 'w';
    EXPECT_EQ(1, ShmNattch(shm.id()));

    const uintptr_t bad_target = ~(static_cast<uintptr_t>(PageSize()) - 1);
    errno = 0;
    void* moved = reinterpret_cast<void*>(
        syscall(SYS_mremap, addr, PageSize(), PageSize() * 2, MREMAP_MAYMOVE | MREMAP_FIXED,
                reinterpret_cast<void*>(bad_target)));
    EXPECT_EQ(MAP_FAILED, moved);
    EXPECT_NE(0, errno);
    EXPECT_EQ(1, ShmNattch(shm.id()));
    EXPECT_EQ('w', addr[0]);

    EXPECT_EQ(0, shmdt(addr));
}

TEST(SysvShmSemantics, MremapFixedRejectsUnalignedTargetBeforeUnmap) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    addr[0] = 'u';
    EXPECT_EQ(1, ShmNattch(shm.id()));

    char* target = static_cast<char*>(
        mmap(nullptr, SegmentSize(), PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, target);
    target[0] = 't';

    errno = 0;
    void* moved = reinterpret_cast<void*>(syscall(
        SYS_mremap, addr, SegmentSize() * 2, PageSize(), MREMAP_MAYMOVE | MREMAP_FIXED,
        target + 1));
    EXPECT_EQ(MAP_FAILED, moved);
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(1, ShmNattch(shm.id()));
    EXPECT_EQ('u', addr[0]);
    EXPECT_EQ('t', target[0]);

    EXPECT_EQ(0, munmap(target, SegmentSize()));
    EXPECT_EQ(0, shmdt(addr));
}

TEST(SysvShmSemantics, MremapFixedReplacesLazyTargetVma) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    const size_t ps = PageSize();
    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    addr[0] = 'm';
    EXPECT_EQ(1, ShmNattch(shm.id()));

    char* target = static_cast<char*>(
        mmap(nullptr, ps, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, target);

    errno = 0;
    void* moved = reinterpret_cast<void*>(
        syscall(SYS_mremap, addr, ps, ps, MREMAP_MAYMOVE | MREMAP_FIXED, target));
    ASSERT_EQ(target, moved) << "mremap fixed failed: errno=" << errno << " ("
                             << strerror(errno) << ")";
    EXPECT_EQ('m', target[0]);
    target[0] = 'M';

    EXPECT_EQ(0, shmdt(addr));
    EXPECT_EQ(0, shmdt(target));
}

TEST(SysvShmSemantics, MremapMissingSourceReturnsEfault) {
    void* addr =
        mmap(nullptr, PageSize(), PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, addr);
    ASSERT_EQ(0, munmap(addr, PageSize()));

    errno = 0;
    void* moved =
        reinterpret_cast<void*>(syscall(SYS_mremap, addr, PageSize(), PageSize(), 0, nullptr));
    EXPECT_EQ(MAP_FAILED, moved);
    EXPECT_EQ(EFAULT, errno);
}

TEST(SysvShmSemantics, MremapRejectsUnknownFlagsWithoutChangingSource) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    addr[0] = 'k';
    EXPECT_EQ(1, ShmNattch(shm.id()));

    errno = 0;
    void* moved = reinterpret_cast<void*>(
        syscall(SYS_mremap, addr, SegmentSize(), SegmentSize(), 0x8, nullptr));
    EXPECT_EQ(MAP_FAILED, moved);
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(1, ShmNattch(shm.id()));
    EXPECT_EQ('k', addr[0]);

    EXPECT_EQ(0, shmdt(addr));
}

TEST(SysvShmSemantics, MremapHugeOldLenFailsWithoutChangingSource) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    addr[0] = 'h';
    EXPECT_EQ(1, ShmNattch(shm.id()));

    const size_t huge_len = ~(PageSize() - 1);
    errno = 0;
    void* moved = reinterpret_cast<void*>(
        syscall(SYS_mremap, addr, huge_len, huge_len, MREMAP_MAYMOVE | MREMAP_DONTUNMAP, nullptr));
    EXPECT_EQ(MAP_FAILED, moved);
    EXPECT_NE(0, errno);
    EXPECT_EQ(1, ShmNattch(shm.id()));
    EXPECT_EQ('h', addr[0]);

    EXPECT_EQ(0, shmdt(addr));
}

TEST(SysvShmSemantics, SysvShmPartialMremapMoveSplitsSource) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    const size_t ps = PageSize();
    addr[0] = 'a';
    addr[ps] = 'b';
    addr[ps * 2] = 'c';
    EXPECT_EQ(1, ShmNattch(shm.id()));

    void* target = mmap(nullptr, ps, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, target);

    void* moved = reinterpret_cast<void*>(
        syscall(SYS_mremap, addr + ps, ps, ps, MREMAP_MAYMOVE | MREMAP_FIXED, target));
    ASSERT_NE(MAP_FAILED, moved) << "partial mremap failed: errno=" << errno << " ("
                                 << strerror(errno) << ")";
    EXPECT_EQ(target, moved);
    EXPECT_EQ('b', static_cast<char*>(moved)[0]);
    EXPECT_EQ('a', addr[0]);
    EXPECT_EQ('c', addr[ps * 2]);
    EXPECT_EQ(3, ShmNattch(shm.id()));
    ExpectChildDiesBySignal(SIGSEGV, WriteFirstByte, addr + ps);

    EXPECT_EQ(0, shmdt(addr));
    EXPECT_EQ(1, ShmNattch(shm.id()));
    EXPECT_EQ(0, shmdt(static_cast<char*>(moved) - ps));
}

TEST(SysvShmSemantics, SysvShmMremapDontunmapNattchPlusOne) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    addr[0] = 'd';
    EXPECT_EQ(1, ShmNattch(shm.id()));

    void* dup = reinterpret_cast<void*>(syscall(
        SYS_mremap, addr, SegmentSize(), SegmentSize(), MREMAP_MAYMOVE | MREMAP_DONTUNMAP, 0));
    ASSERT_NE(MAP_FAILED, dup) << "mremap DONTUNMAP failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    EXPECT_EQ('d', static_cast<char*>(dup)[0]);
    static_cast<char*>(dup)[0] = 'e';
    EXPECT_EQ('e', addr[0]);
    EXPECT_EQ(2, ShmNattch(shm.id()));

    EXPECT_EQ(0, shmdt(addr));
    EXPECT_EQ(1, ShmNattch(shm.id()));
    EXPECT_EQ(0, shmdt(dup));
}

TEST(SysvShmSemantics, SysvShmPartialMremapDontunmapKeepsSourceVma) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    const size_t ps = PageSize();
    addr[0] = 'x';
    addr[ps] = 'y';
    addr[ps * 2] = 'z';
    EXPECT_EQ(1, ShmNattch(shm.id()));

    void* dup = reinterpret_cast<void*>(syscall(
        SYS_mremap, addr + ps, ps, ps, MREMAP_MAYMOVE | MREMAP_DONTUNMAP, 0));
    ASSERT_NE(MAP_FAILED, dup) << "partial mremap DONTUNMAP failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    EXPECT_EQ('y', static_cast<char*>(dup)[0]);
    static_cast<char*>(dup)[0] = 'q';
    EXPECT_EQ('q', addr[ps]);
    addr[ps] = 'r';
    EXPECT_EQ('r', static_cast<char*>(dup)[0]);
    EXPECT_EQ('x', addr[0]);
    EXPECT_EQ('z', addr[ps * 2]);
    EXPECT_EQ(2, ShmNattch(shm.id()));

    EXPECT_EQ(0, shmdt(addr));
    EXPECT_EQ(1, ShmNattch(shm.id()));
    EXPECT_EQ(0, shmdt(static_cast<char*>(dup) - ps));
}

TEST(SysvShmSemantics, LockedPartialMremapDontunmapDoesNotResplitSource) {
    ScopedMemlockLimit lim;
    ASSERT_TRUE(lim.valid());
    ASSERT_TRUE(lim.set_bytes(SegmentSize() * 2));

    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    const size_t ps = PageSize();
    addr[ps] = 'l';
    ASSERT_EQ(0, mlock(addr, SegmentSize()));
    EXPECT_EQ(1, ShmNattch(shm.id()));

    void* dup = reinterpret_cast<void*>(syscall(
        SYS_mremap, addr + ps, ps, ps, MREMAP_MAYMOVE | MREMAP_DONTUNMAP, 0));
    ASSERT_NE(MAP_FAILED, dup) << "locked partial mremap DONTUNMAP failed: errno=" << errno
                               << " (" << strerror(errno) << ")";

    EXPECT_EQ('l', static_cast<char*>(dup)[0]);
    static_cast<char*>(dup)[0] = 'L';
    EXPECT_EQ('L', addr[ps]);
    EXPECT_EQ(2, ShmNattch(shm.id()))
        << "locked MREMAP_DONTUNMAP must not split the SysV SHM source VMA";

    EXPECT_EQ(0, shmdt(addr));
    EXPECT_EQ(1, ShmNattch(shm.id()));
    EXPECT_EQ(0, shmdt(static_cast<char*>(dup) - ps));
}

TEST(SysvShmSemantics, SysvShmMremapOldLenZeroDuplicate) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    addr[0] = 'z';

    void* dup =
        reinterpret_cast<void*>(syscall(SYS_mremap, addr, 0, SegmentSize(), MREMAP_MAYMOVE));
    ASSERT_NE(MAP_FAILED, dup) << "mremap duplicate failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    EXPECT_EQ('z', static_cast<char*>(dup)[0]);
    static_cast<char*>(dup)[0] = 'w';
    EXPECT_EQ('w', addr[0]);
    EXPECT_EQ(2, ShmNattch(shm.id()));

    EXPECT_EQ(0, shmdt(dup));
    EXPECT_EQ(0, shmdt(addr));
}

TEST(SysvShmSemantics, SysvShmMremapOldLenZeroKeepsSourceMlockAccounting) {
    ScopedMemlockLimit lim;
    ASSERT_TRUE(lim.valid());
    ASSERT_TRUE(lim.set_bytes(SegmentSize() * 2));

    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    ASSERT_EQ(0, mlock(addr, SegmentSize()));

    void* dup =
        reinterpret_cast<void*>(syscall(SYS_mremap, addr, 0, SegmentSize(), MREMAP_MAYMOVE));
    ASSERT_NE(MAP_FAILED, dup) << "mremap duplicate failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    errno = 0;
    EXPECT_EQ(-1, msync(dup, SegmentSize(), MS_INVALIDATE));
    EXPECT_EQ(EBUSY, errno);

    EXPECT_EQ(0, munlock(dup, SegmentSize()));

    errno = 0;
    EXPECT_EQ(-1, msync(addr, SegmentSize(), MS_INVALIDATE));
    EXPECT_EQ(EBUSY, errno);

    EXPECT_EQ(0, munlock(addr, SegmentSize()));

    void* probe = mmap(nullptr, SegmentSize() * 2, PROT_READ | PROT_WRITE,
                       MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, probe);
    EXPECT_EQ(0, mlock(probe, SegmentSize() * 2))
        << "SysV old_len=0 duplicate leaked locked_vm accounting, errno=" << errno << " ("
        << strerror(errno) << ")";
    EXPECT_EQ(0, munlock(probe, SegmentSize() * 2));
    EXPECT_EQ(0, munmap(probe, SegmentSize() * 2));

    EXPECT_EQ(0, shmdt(dup));
    EXPECT_EQ(0, shmdt(addr));
}

TEST(SysvShmSemantics, SysvShmMremapOldLenZeroFromMiddleAllowsLongNewLen) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    const size_t ps = PageSize();
    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    addr[ps] = 'u';
    addr[ps * 2] = 'v';

    void* dup =
        reinterpret_cast<void*>(syscall(SYS_mremap, addr + ps, 0, SegmentSize(), MREMAP_MAYMOVE));
    ASSERT_NE(MAP_FAILED, dup) << "mremap duplicate from middle failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    EXPECT_EQ('u', static_cast<char*>(dup)[0]);
    EXPECT_EQ('v', static_cast<char*>(dup)[ps]);
    static_cast<char*>(dup)[0] = 'U';
    EXPECT_EQ('U', addr[ps]);
    EXPECT_EQ(2, ShmNattch(shm.id()));

    EXPECT_EQ(0, shmdt(static_cast<char*>(dup) - ps));
    EXPECT_EQ(0, shmdt(addr));
}

TEST(SysvShmSemantics, SysvShmMremapFailureDoesNotLeakAttach) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    addr[0] = 'f';
    EXPECT_EQ(1, ShmNattch(shm.id()));

    errno = 0;
    void* failed =
        reinterpret_cast<void*>(syscall(SYS_mremap, addr, SegmentSize(), SegmentSize(),
                                        MREMAP_DONTUNMAP, 0));
    EXPECT_EQ(MAP_FAILED, failed);
    EXPECT_EQ(1, ShmNattch(shm.id()));
    EXPECT_EQ('f', addr[0]);

    EXPECT_EQ(0, shmdt(addr));
}

TEST(SysvShmSemantics, IpcRmidKeepsOldIdVisibleUntilLastDetach) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());
    const int id = shm.release();

    char* addr = static_cast<char*>(Attach(id));
    ASSERT_NE(MAP_FAILED, addr);
    addr[0] = 'r';
    EXPECT_EQ(1, ShmNattch(id));

    struct shmid_ds ds;
    ASSERT_EQ(0, shmctl(id, IPC_STAT, &ds));
    const time_t original_ctime = ds.shm_ctime;
    sleep(1);

    ASSERT_EQ(0, shmctl(id, IPC_RMID, nullptr));

    ASSERT_EQ(0, shmctl(id, IPC_STAT, &ds));
    EXPECT_EQ(1U, ds.shm_nattch);
    EXPECT_NE(0, ds.shm_perm.mode & SHM_DEST);
    EXPECT_EQ(original_ctime, ds.shm_ctime);

    char* again = static_cast<char*>(Attach(id));
    ASSERT_NE(MAP_FAILED, again);
    EXPECT_EQ('r', again[0]);

    EXPECT_EQ('r', addr[0]);
    addr[0] = 'R';
    EXPECT_EQ('R', again[0]);
    EXPECT_EQ(0, shmdt(again));
    EXPECT_EQ(0, shmdt(addr));

    errno = 0;
    void* denied = shmat(id, nullptr, 0);
    EXPECT_EQ(reinterpret_cast<void*>(-1), denied);
    EXPECT_EQ(EINVAL, errno);
}

TEST(SysvShmSemantics, IpcRmidReleasesKeyWhileOldMappingDetachesByTombstone) {
    const key_t key = UniqueKey();
    ShmSegment first(key, SegmentSize(), IPC_CREAT | IPC_EXCL | 0600);
    ASSERT_TRUE(first.valid()) << "first shmget failed: errno=" << errno << " (" << strerror(errno)
                               << ")";
    const int old_id = first.release();

    char* old_addr = static_cast<char*>(Attach(old_id));
    ASSERT_NE(MAP_FAILED, old_addr);
    old_addr[0] = 'o';
    ASSERT_EQ(0, shmctl(old_id, IPC_RMID, nullptr));

    ShmSegment second(key, SegmentSize(), IPC_CREAT | IPC_EXCL | 0600);
    ASSERT_TRUE(second.valid()) << "key was not released by IPC_RMID: errno=" << errno << " ("
                                << strerror(errno) << ")";
    EXPECT_NE(old_id, second.id());

    char* old_again = static_cast<char*>(Attach(old_id));
    ASSERT_NE(MAP_FAILED, old_again);
    EXPECT_EQ('o', old_again[0]);

    EXPECT_EQ('o', old_addr[0]);
    EXPECT_EQ(0, shmdt(old_again));
    EXPECT_EQ(0, shmdt(old_addr));
}

TEST(SysvShmSemantics, SysvShmContentSurvivesDropCachesAfterDetach) {
    ShmSegment shm(SegmentSize());
    ASSERT_TRUE(shm.valid());
    const size_t ps = PageSize();

    char* addr = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, addr);
    addr[0] = 'a';
    addr[ps] = 'b';
    addr[ps * 2] = 'c';
    ASSERT_EQ(0, msync(addr, SegmentSize(), MS_SYNC))
        << "msync failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_EQ(0, shmdt(addr));

    DropPageCache();

    char* again = static_cast<char*>(Attach(shm.id()));
    ASSERT_NE(MAP_FAILED, again);
    EXPECT_EQ('a', again[0]);
    EXPECT_EQ('b', again[ps]);
    EXPECT_EQ('c', again[ps * 2]);
    EXPECT_EQ(0, shmdt(again));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
