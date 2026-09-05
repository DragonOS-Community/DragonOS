#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <memory>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/sem.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <vector>

#ifndef SYS_semget
#define SYS_semget 64
#endif
#ifndef SYS_semop
#define SYS_semop 65
#endif
#ifndef SYS_semctl
#define SYS_semctl 66
#endif
#ifndef SYS_semtimedop
#define SYS_semtimedop 220
#endif
#ifndef SYS_unshare
#define SYS_unshare 272
#endif
#ifndef SYS_setns
#define SYS_setns 308
#endif
#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif

#ifndef SEM_STAT
#define SEM_STAT 18
#endif
#ifndef SEM_STAT_ANY
#define SEM_STAT_ANY 20
#endif
#ifndef CLONE_SYSVSEM
#define CLONE_SYSVSEM 0x00040000
#endif
#ifndef CLONE_NEWIPC
#define CLONE_NEWIPC 0x08000000
#endif

// ============ helpers ============

int SemGet(key_t key, int nsems, int flags) {
    return static_cast<int>(syscall(SYS_semget, key, nsems, flags));
}

int SemCtl(int semid, int semnum, int cmd, unsigned long arg) {
    return static_cast<int>(syscall(SYS_semctl, semid, semnum, cmd, arg));
}

int SemOp(int semid, struct sembuf* sops, size_t nsops) {
    return static_cast<int>(syscall(SYS_semop, semid, sops, nsops));
}

int SemTimedOp(int semid, struct sembuf* sops, size_t nsops, const struct timespec* timeout) {
    return static_cast<int>(syscall(SYS_semtimedop, semid, sops, nsops, timeout));
}

key_t UniqueKey() {
    static int seq = 0;
    return static_cast<key_t>(0x53000000 ^ (getpid() << 8) ^ (++seq));
}

class SemSet {
  public:
    SemSet(key_t key, int nsems, int flags) : id_(SemGet(key, nsems, flags)) {}

    SemSet(int nsems, int flags) : id_(SemGet(IPC_PRIVATE, nsems, flags)) {}

    // Adopt an existing ID
    SemSet(int existing_id, bool owns) : id_(existing_id), owns_(owns) {}

    ~SemSet() {
        if (id_ >= 0 && owns_) {
            SemCtl(id_, 0, IPC_RMID, 0);
        }
    }

    SemSet(const SemSet&) = delete;
    SemSet& operator=(const SemSet&) = delete;

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

class ChildGuard {
  public:
    explicit ChildGuard(pid_t pid) : pid_(pid) {}

    ~ChildGuard() {
        if (pid_ <= 0) {
            return;
        }
        kill(pid_, SIGKILL);
        while (waitpid(pid_, nullptr, 0) < 0 && errno == EINTR) {
        }
    }

    ChildGuard(const ChildGuard&) = delete;
    ChildGuard& operator=(const ChildGuard&) = delete;

    pid_t pid() const {
        return pid_;
    }

    void MarkReaped() {
        pid_ = -1;
    }

  private:
    pid_t pid_;
};

class ScopedAffinity {
  public:
    bool PinToFirstCpu() {
        CPU_ZERO(&saved_);
        if (sched_getaffinity(0, sizeof(saved_), &saved_) != 0) {
            return false;
        }

        cpu_set_t target;
        CPU_ZERO(&target);
        for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
            if (CPU_ISSET(cpu, &saved_)) {
                CPU_SET(cpu, &target);
                active_ = sched_setaffinity(0, sizeof(target), &target) == 0;
                return active_;
            }
        }
        return false;
    }

    ~ScopedAffinity() {
        if (active_ && sched_setaffinity(0, sizeof(saved_), &saved_) != 0) {
            ADD_FAILURE() << "failed to restore affinity: errno=" << errno << " ("
                          << strerror(errno) << ")";
        }
    }

    ScopedAffinity(const ScopedAffinity&) = delete;
    ScopedAffinity& operator=(const ScopedAffinity&) = delete;
    ScopedAffinity() = default;

  private:
    cpu_set_t saved_ = {};
    bool active_ = false;
};

class FdGuard {
  public:
    explicit FdGuard(int fd) : fd_(fd) {}

    ~FdGuard() {
        Close();
    }

    FdGuard(const FdGuard&) = delete;
    FdGuard& operator=(const FdGuard&) = delete;

    int get() const {
        return fd_;
    }

    void Close() {
        if (fd_ >= 0) {
            close(fd_);
            fd_ = -1;
        }
    }

  private:
    int fd_;
};

void WaitChildOk(pid_t child) {
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0))
        << "waitpid failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_TRUE(WIFEXITED(status)) << "child did not exit normally, status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status)) << "child failed, status=" << status;
}

void WaitChildOk(ChildGuard* child) {
    const pid_t pid = child->pid();
    int status = 0;
    pid_t waited;
    do {
        waited = waitpid(pid, &status, 0);
    } while (waited < 0 && errno == EINTR);
    if (waited == pid) {
        child->MarkReaped();
    }
    ASSERT_EQ(pid, waited)
        << "waitpid failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_TRUE(WIFEXITED(status)) << "child did not exit normally, status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status)) << "child failed, status=" << status;
}

// Wait until the target semaphore has the expected number of waiters, confirming that
// child processes have blocked. GETNCNT and GETZCNT are the semaphore ABI handshake.
// A bounded exponential pause prevents a tight spin; correctness depends only on the
// observed counter and a monotonic deadline, never on a guessed child run interval.
bool WaitForWaiters(int semid, int semnum, int expected, int timeout_ms = 5000) {
    struct timespec start = {};
    clock_gettime(CLOCK_MONOTONIC, &start);
    long pause_ns = 1000;
    for (;;) {
        int ncnt = SemCtl(semid, semnum, GETNCNT, 0);
        int zcnt = SemCtl(semid, semnum, GETZCNT, 0);
        if (ncnt >= 0 && ncnt + zcnt >= expected) {
            return true;
        }
        struct timespec now = {};
        clock_gettime(CLOCK_MONOTONIC, &now);
        const long elapsed_ms = (now.tv_sec - start.tv_sec) * 1000 +
                                (now.tv_nsec - start.tv_nsec) / 1000000;
        if (elapsed_ms >= timeout_ms) {
            return false;
        }
        const struct timespec pause = {0, pause_ns};
        nanosleep(&pause, nullptr);
        if (pause_ns < 1000000) {
            pause_ns *= 2;
        }
    }
}

bool ReadExact(int fd, void* buffer, size_t size) {
    char* cursor = static_cast<char*>(buffer);
    while (size != 0) {
        ssize_t received = read(fd, cursor, size);
        if (received < 0 && errno == EINTR) {
            continue;
        }
        if (received <= 0) {
            return false;
        }
        cursor += received;
        size -= static_cast<size_t>(received);
    }
    return true;
}

bool WriteExact(int fd, const void* buffer, size_t size) {
    const char* cursor = static_cast<const char*>(buffer);
    while (size != 0) {
        ssize_t written = write(fd, cursor, size);
        if (written < 0 && errno == EINTR) {
            continue;
        }
        if (written <= 0) {
            return false;
        }
        cursor += written;
        size -= static_cast<size_t>(written);
    }
    return true;
}

bool WaitForNcnt(int semid, int semnum, int expected) {
    return WaitForWaiters(semid, semnum, expected) &&
           SemCtl(semid, semnum, GETNCNT, 0) >= expected;
}

bool WaitForZcnt(int semid, int semnum, int expected) {
    return WaitForWaiters(semid, semnum, expected) &&
           SemCtl(semid, semnum, GETZCNT, 0) >= expected;
}

bool WaitForSemValue(int semid, int semnum, int expected, int timeout_ms = 5000) {
    struct timespec start = {};
    clock_gettime(CLOCK_MONOTONIC, &start);
    long pause_ns = 1000;
    for (;;) {
        if (SemCtl(semid, semnum, GETVAL, 0) == expected) {
            return true;
        }
        struct timespec now = {};
        clock_gettime(CLOCK_MONOTONIC, &now);
        const long elapsed_ms = (now.tv_sec - start.tv_sec) * 1000 +
                                (now.tv_nsec - start.tv_nsec) / 1000000;
        if (elapsed_ms >= timeout_ms) {
            return false;
        }
        const struct timespec pause = {0, pause_ns};
        nanosleep(&pause, nullptr);
        if (pause_ns < 1000000) {
            pause_ns *= 2;
        }
    }
}

bool SemOpMustSucceed(int semid, struct sembuf* ops, size_t count) {
    return SemOp(semid, ops, count) == 0;
}

bool SemUndoOpMustSucceed(int semid, unsigned short semnum, short delta) {
    struct sembuf op = {semnum, delta, SEM_UNDO};
    return SemOpMustSucceed(semid, &op, 1);
}

// ============ creation & lookup ============

TEST(SysVSem, CreatePrivateSet) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid()) << "semget(IPC_PRIVATE) failed: errno=" << errno;
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0));
}

TEST(SysVSem, CreateMultipleSems) {
    SemSet sem(4, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    for (int i = 0; i < 4; ++i) {
        EXPECT_EQ(0, SemCtl(sem.id(), i, GETVAL, 0)) << "sem " << i;
    }
    EXPECT_EQ(-1, SemCtl(sem.id(), 4, GETVAL, 0));
    EXPECT_EQ(EINVAL, errno) << "GETVAL with out-of-range semnum";
}

TEST(SysVSem, KeyedCreateAndLookup) {
    const key_t key = UniqueKey();
    const int id1 = SemGet(key, 1, IPC_CREAT | IPC_EXCL | 0600);
    ASSERT_GE(id1, 0) << "first semget failed: errno=" << errno;
    SemSet guard(id1, true /* owns */);

    const int id2 = SemGet(key, 1, IPC_CREAT | 0600);
    ASSERT_GE(id2, 0) << "lookup failed: errno=" << errno;
    EXPECT_EQ(id1, id2) << "same key must map to same id";
}

TEST(SysVSem, ExistingKeyAllowsZeroNsems) {
    const key_t key = UniqueKey();
    SemSet sem(key, 2, IPC_CREAT | IPC_EXCL | 0600);
    ASSERT_TRUE(sem.valid()) << "creation failed: errno=" << errno;

    EXPECT_EQ(sem.id(), SemGet(key, 0, 0)) << "existing lookup must allow nsems == 0";
}

TEST(SysVSem, NegativeKeyPreservesLow32Bits) {
    const key_t key = -((UniqueKey() & 0x3fffffff) | 1);
    SemSet sem(key, 1, IPC_CREAT | IPC_EXCL | 0600);
    ASSERT_TRUE(sem.valid()) << "negative key creation failed: errno=" << errno;

    EXPECT_EQ(sem.id(), SemGet(key, 0, 0)) << "negative key lookup must find the same set";
}

TEST(SysVSem, KeyedCreateExclConflict) {
    const key_t key = UniqueKey();
    const int id = SemGet(key, 1, IPC_CREAT | IPC_EXCL | 0600);
    ASSERT_GE(id, 0);
    SemSet guard(id, true);

    errno = 0;
    EXPECT_EQ(-1, SemGet(key, 1, IPC_CREAT | IPC_EXCL | 0600));
    EXPECT_EQ(EEXIST, errno);
}

TEST(SysVSem, LookupNoCreat) {
    errno = 0;
    EXPECT_EQ(-1, SemGet(UniqueKey(), 1, 0600));
    EXPECT_EQ(ENOENT, errno);
}

TEST(SysVSem, CreateLargerNsemsFails) {
    const key_t key = UniqueKey();
    const int id = SemGet(key, 2, IPC_CREAT | IPC_EXCL | 0600);
    ASSERT_GE(id, 0);
    SemSet guard(id, true);

    errno = 0;
    EXPECT_EQ(-1, SemGet(key, 3, IPC_CREAT | 0600));
    EXPECT_EQ(EINVAL, errno) << "requesting more sems than existing set";

    errno = 0;
    EXPECT_EQ(-1, SemGet(key, 32001, 0));
    EXPECT_EQ(EINVAL, errno) << "nsems > SEMMSL must fail even for an existing set";
}

TEST(SysVSem, InvalidNsems) {
    errno = 0;
    EXPECT_EQ(-1, SemGet(IPC_PRIVATE, 0, IPC_CREAT | 0600));
    EXPECT_EQ(EINVAL, errno) << "nsems == 0";

    errno = 0;
    EXPECT_EQ(-1, SemGet(IPC_PRIVATE, 32001, IPC_CREAT | 0600));
    EXPECT_EQ(EINVAL, errno) << "nsems > SEMMSL";
}

// ============ semctl ============

TEST(SysVSem, IpcStatFields) {
    SemSet sem(2, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    struct semid_ds ds;
    ASSERT_EQ(0, SemCtl(sem.id(), 0, IPC_STAT, reinterpret_cast<unsigned long>(&ds)))
        << "IPC_STAT failed: errno=" << errno;
    EXPECT_EQ(2u, ds.sem_nsems);
    EXPECT_EQ(0, ds.sem_otime) << "otime must be 0 before any semop";
    EXPECT_GT(ds.sem_ctime, 0) << "ctime set at creation";
    EXPECT_EQ(0600, static_cast<int>(ds.sem_perm.mode & 0777));
}

TEST(SysVSem, SemStatAndSemStatAny) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    const int index = sem.id() & 0x7fff;
    struct semid_ds ds;
    int ret = SemCtl(index, INT_MAX, SEM_STAT, reinterpret_cast<unsigned long>(&ds));
    ASSERT_GE(ret, 0) << "SEM_STAT failed: errno=" << errno;
    EXPECT_EQ(sem.id(), ret) << "SEM_STAT must return the full semid";
    EXPECT_EQ(1u, ds.sem_nsems);

    ret = SemCtl(index, INT_MAX, SEM_STAT_ANY, reinterpret_cast<unsigned long>(&ds));
    ASSERT_GE(ret, 0) << "SEM_STAT_ANY failed: errno=" << errno;
    EXPECT_EQ(sem.id(), ret) << "SEM_STAT_ANY must return the full semid";
    EXPECT_EQ(1u, ds.sem_nsems);
}

TEST(SysVSem, IpcSetMode) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    struct semid_ds ds;
    ASSERT_EQ(0, SemCtl(sem.id(), 0, IPC_STAT, reinterpret_cast<unsigned long>(&ds)));
    ds.sem_perm.mode = 0644;
    ASSERT_EQ(0, SemCtl(sem.id(), 0, IPC_SET, reinterpret_cast<unsigned long>(&ds)))
        << "IPC_SET failed: errno=" << errno;

    memset(&ds, 0, sizeof(ds));
    ASSERT_EQ(0, SemCtl(sem.id(), 0, IPC_STAT, reinterpret_cast<unsigned long>(&ds)));
    EXPECT_EQ(0644, static_cast<int>(ds.sem_perm.mode & 0777));
}

TEST(SysVSem, IpcSetInvalidGidDoesNotPartiallyUpdate) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root to inspect the set after transferring ownership";
    }

    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    struct semid_ds before = {};
    ASSERT_EQ(0, SemCtl(sem.id(), 0, IPC_STAT, reinterpret_cast<unsigned long>(&before)));

    struct semid_ds update = before;
    update.sem_perm.uid = 1234;
    update.sem_perm.gid = UINT32_MAX;
    update.sem_perm.mode = 0644;

    errno = 0;
    EXPECT_EQ(-1, SemCtl(sem.id(), 0, IPC_SET, reinterpret_cast<unsigned long>(&update)));
    EXPECT_EQ(EINVAL, errno);

    struct semid_ds after = {};
    ASSERT_EQ(0, SemCtl(sem.id(), 0, IPC_STAT, reinterpret_cast<unsigned long>(&after)));
    EXPECT_EQ(before.sem_perm.uid, after.sem_perm.uid);
    EXPECT_EQ(before.sem_perm.gid, after.sem_perm.gid);
    EXPECT_EQ(before.sem_perm.mode & 0777, after.sem_perm.mode & 0777);
}

TEST(SysVSem, OwnerCanControlModeZeroSet) {
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (setgid(1000) != 0 || setuid(1000) != 0) {
            _exit(20);
        }

        SemSet set_target(1, IPC_CREAT | 0000);
        if (!set_target.valid()) {
            _exit(21);
        }
        struct semid_ds ds = {};
        ds.sem_perm.uid = geteuid();
        ds.sem_perm.gid = getegid();
        ds.sem_perm.mode = 0400;
        if (SemCtl(set_target.id(), 0, IPC_SET, reinterpret_cast<unsigned long>(&ds)) != 0) {
            _exit(22);
        }

        SemSet remove_target(1, IPC_CREAT | 0000);
        if (!remove_target.valid()) {
            _exit(23);
        }
        if (SemCtl(remove_target.id(), 0, IPC_RMID, 0) != 0) {
            _exit(24);
        }
        remove_target.release();
        _exit(0);
    }
    WaitChildOk(child);
}

TEST(SysVSem, IpcInfoAndSemInfo) {
    struct seminfo info;
    memset(&info, 0, sizeof(info));
    int max_id = SemCtl(0, 0, IPC_INFO, reinterpret_cast<unsigned long>(&info));
    EXPECT_GE(max_id, 0);
    EXPECT_EQ(32000 * 32000, info.semmap);
    EXPECT_EQ(500, info.semopm);
    EXPECT_EQ(500, info.semume);
    EXPECT_EQ(20, info.semusz);
    EXPECT_EQ(32767, info.semvmx);
    EXPECT_EQ(32767, info.semaem);

    struct seminfo before = {};
    ASSERT_GE(SemCtl(0, 0, SEM_INFO, reinterpret_cast<unsigned long>(&before)), 0);

    SemSet first(2, IPC_CREAT | 0600);
    SemSet second(3, IPC_CREAT | 0600);
    ASSERT_TRUE(first.valid());
    ASSERT_TRUE(second.valid());

    memset(&info, 0, sizeof(info));
    max_id = SemCtl(0, 0, SEM_INFO, reinterpret_cast<unsigned long>(&info));
    EXPECT_GE(max_id, 0);
    EXPECT_EQ(before.semusz + 2, info.semusz) << "SEM_INFO semusz is the set count";
    EXPECT_EQ(before.semaem + 5, info.semaem) << "SEM_INFO semaem is the semaphore count";
}

TEST(SysVSem, KeyedCreateRemovalRestoresAccountingAndAllowsReuse) {
    struct seminfo before = {};
    ASSERT_GE(SemCtl(0, 0, SEM_INFO, reinterpret_cast<unsigned long>(&before)), 0);

    const key_t key = UniqueKey();
    int semid = SemGet(key, 4, IPC_CREAT | IPC_EXCL | 0600);
    ASSERT_GE(semid, 0) << "semget failed: errno=" << errno;
    EXPECT_EQ(semid, SemGet(key, 0, 0));

    struct seminfo created = {};
    ASSERT_GE(SemCtl(0, 0, SEM_INFO, reinterpret_cast<unsigned long>(&created)), 0);
    EXPECT_EQ(before.semusz + 1, created.semusz);
    EXPECT_EQ(before.semaem + 4, created.semaem);

    ASSERT_EQ(0, SemCtl(semid, 0, IPC_RMID, 0));
    errno = 0;
    EXPECT_EQ(-1, SemGet(key, 0, 0));
    EXPECT_EQ(ENOENT, errno);

    struct seminfo removed = {};
    ASSERT_GE(SemCtl(0, 0, SEM_INFO, reinterpret_cast<unsigned long>(&removed)), 0);
    EXPECT_EQ(before.semusz, removed.semusz);
    EXPECT_EQ(before.semaem, removed.semaem);

    SemSet reused(key, 4, IPC_CREAT | IPC_EXCL | 0600);
    ASSERT_TRUE(reused.valid()) << "key must be reusable after IPC_RMID: errno=" << errno;
}

TEST(SysVSem, SetValGetVal) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 5)) << "SETVAL failed: errno=" << errno;
    EXPECT_EQ(5, SemCtl(sem.id(), 0, GETVAL, 0));
}

TEST(SysVSem, SetValRangeErrors) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    errno = 0;
    EXPECT_EQ(-1, SemCtl(sem.id(), 0, SETVAL, 32768));
    EXPECT_EQ(ERANGE, errno) << "SETVAL > SEMVMX";

    errno = 0;
    EXPECT_EQ(-1, SemCtl(sem.id(), 0, SETVAL, -1));
    EXPECT_EQ(ERANGE, errno) << "SETVAL < 0";
}

TEST(SysVSem, SetAllGetAll) {
    SemSet sem(3, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    unsigned short vals[3] = {1, 2, 3};
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETALL, reinterpret_cast<unsigned long>(vals)))
        << "SETALL failed: errno=" << errno;

    unsigned short out[3] = {0, 0, 0};
    ASSERT_EQ(0, SemCtl(sem.id(), 0, GETALL, reinterpret_cast<unsigned long>(out)));
    EXPECT_EQ(1u, out[0]);
    EXPECT_EQ(2u, out[1]);
    EXPECT_EQ(3u, out[2]);
}

TEST(SysVSem, LargeSetAllGetAll) {
    constexpr size_t count = 32000;
    SemSet sem(count, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid()) << "errno=" << errno;
    std::vector<unsigned short> values(count), actual(count);
    for (size_t i = 0; i < count; ++i)
        values[i] = static_cast<unsigned short>(i % 32768);
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETALL,
                        reinterpret_cast<unsigned long>(values.data())));
    ASSERT_EQ(0, SemCtl(sem.id(), 0, GETALL,
                        reinterpret_cast<unsigned long>(actual.data())));
    EXPECT_EQ(values, actual);
}

TEST(SysVSem, SetAllAtomicRangeError) {
    SemSet sem(3, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    unsigned short ok[3] = {1, 2, 3};
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETALL, reinterpret_cast<unsigned long>(ok)));

    unsigned short bad[3] = {1, 40000, 3};
    errno = 0;
    EXPECT_EQ(-1, SemCtl(sem.id(), 0, SETALL, reinterpret_cast<unsigned long>(bad)));
    EXPECT_EQ(ERANGE, errno);

    // Validation failure must not change any value.
    unsigned short out[3] = {0, 0, 0};
    ASSERT_EQ(0, SemCtl(sem.id(), 0, GETALL, reinterpret_cast<unsigned long>(out)));
    EXPECT_EQ(1u, out[0]);
    EXPECT_EQ(2u, out[1]);
    EXPECT_EQ(3u, out[2]);
}

TEST(SysVSem, SetAllValidatesSetAndPermissionBeforeUserArray) {
    SemSet sem(1, IPC_CREAT | 0000);
    ASSERT_TRUE(sem.valid());

    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (setgid(1000) != 0 || setuid(1000) != 0) {
            _exit(20);
        }
        errno = 0;
        if (SemCtl(sem.id(), 0, SETALL, 1) != -1 || errno != EACCES) {
            _exit(21);
        }
        _exit(0);
    }
    WaitChildOk(child);

    errno = 0;
    EXPECT_EQ(-1, SemCtl(0x3FFFFFFF, 0, SETALL, 1));
    EXPECT_EQ(EINVAL, errno) << "object existence must be checked before the user pointer";
}

TEST(SysVSem, GetPidTracksLastSemop) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    struct sembuf op = {0, 1, 0};
    ASSERT_EQ(0, SemOp(sem.id(), &op, 1)) << "semop failed: errno=" << errno;
    EXPECT_EQ(getpid(), SemCtl(sem.id(), 0, GETPID, 0)) << "GETPID must be current pid";

    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 3));
    EXPECT_EQ(getpid(), SemCtl(sem.id(), 0, GETPID, 0)) << "SETVAL also updates sempid";
}

TEST(SysVSem, GetPidPreservesSemUndoActorAfterWaitpidReap) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    int ready_pipe[2];
    int release_pipe[2];
    ASSERT_EQ(0, pipe(ready_pipe));
    ASSERT_EQ(0, pipe(release_pipe));
    FdGuard ready_read(ready_pipe[0]);
    FdGuard ready_write(ready_pipe[1]);
    FdGuard release_read(release_pipe[0]);
    FdGuard release_write(release_pipe[1]);

    const pid_t child_pid = fork();
    ASSERT_GE(child_pid, 0);
    if (child_pid == 0) {
        close(ready_pipe[0]);
        close(release_pipe[1]);
        char byte = 1;
        if (!SemUndoOpMustSucceed(sem.id(), 0, 1) ||
            !WriteExact(ready_pipe[1], &byte, sizeof(byte)) ||
            !ReadExact(release_pipe[0], &byte, sizeof(byte))) {
            _exit(1);
        }
        _exit(0);
    }
    ChildGuard child(child_pid);
    ready_write.Close();
    release_read.Close();

    char byte = 0;
    ASSERT_TRUE(ReadExact(ready_read.get(), &byte, sizeof(byte)));
    EXPECT_EQ(1, SemCtl(sem.id(), 0, GETVAL, 0));

    struct sembuf parent_op = {0, 1, 0};
    ASSERT_EQ(0, SemOp(sem.id(), &parent_op, 1));
    EXPECT_EQ(getpid(), SemCtl(sem.id(), 0, GETPID, 0));

    ASSERT_TRUE(WriteExact(release_write.get(), &byte, sizeof(byte)));
    WaitChildOk(&child);
    EXPECT_EQ(1, SemCtl(sem.id(), 0, GETVAL, 0)) << "SEM_UNDO must preserve the parent delta";
    EXPECT_EQ(child_pid, SemCtl(sem.id(), 0, GETPID, 0))
        << "GETPID must retain the reaped SEM_UNDO actor identity";
}

// ============ semop semantics ============

TEST(SysVSem, IncrementDecrement) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    struct sembuf inc = {0, 2, 0};
    ASSERT_EQ(0, SemOp(sem.id(), &inc, 1));
    EXPECT_EQ(2, SemCtl(sem.id(), 0, GETVAL, 0));

    struct sembuf dec = {0, -1, 0};
    ASSERT_EQ(0, SemOp(sem.id(), &dec, 1));
    EXPECT_EQ(1, SemCtl(sem.id(), 0, GETVAL, 0));
}

TEST(SysVSem, IncrementOverflowIsErange) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 32766));

    // semval + op > SEMVMX: return ERANGE immediately without blocking or modifying values.
    struct sembuf inc = {0, 2, 0};
    errno = 0;
    EXPECT_EQ(-1, SemOp(sem.id(), &inc, 1));
    EXPECT_EQ(ERANGE, errno);
    EXPECT_EQ(32766, SemCtl(sem.id(), 0, GETVAL, 0)) << "no change on ERANGE";
}

TEST(SysVSem, WaitForZero) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    // val == 0: succeed immediately
    struct sembuf op = {0, 0, 0};
    ASSERT_EQ(0, SemOp(sem.id(), &op, 1));

    // val != 0 + IPC_NOWAIT：EAGAIN
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 3));
    struct sembuf op_nowait = {0, 0, IPC_NOWAIT};
    errno = 0;
    EXPECT_EQ(-1, SemOp(sem.id(), &op_nowait, 1));
    EXPECT_EQ(EAGAIN, errno);
}

TEST(SysVSem, DecrementBelowZeroNowait) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    struct sembuf op = {0, -1, IPC_NOWAIT};
    errno = 0;
    EXPECT_EQ(-1, SemOp(sem.id(), &op, 1));
    EXPECT_EQ(EAGAIN, errno);
}

TEST(SysVSem, AtomicMultiOpRollback) {
    SemSet sem(2, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 1));

    // op1 succeeds (1 - 1 = 0), then op2 fails (0 - 1 < 0, IPC_NOWAIT): the whole
    // group returns EAGAIN and rolls back op1.
    struct sembuf ops[2] = {{0, -1, 0}, {1, -1, IPC_NOWAIT}};
    errno = 0;
    EXPECT_EQ(-1, SemOp(sem.id(), ops, 2));
    EXPECT_EQ(EAGAIN, errno);

    EXPECT_EQ(1, SemCtl(sem.id(), 0, GETVAL, 0)) << "op1 must be rolled back";
}

TEST(SysVSem, AtomicMultiOpAllSucceed) {
    SemSet sem(2, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 2));
    ASSERT_EQ(0, SemCtl(sem.id(), 1, SETVAL, 5));

    struct sembuf ops[2] = {{0, -1, 0}, {1, 3, 0}};
    ASSERT_EQ(0, SemOp(sem.id(), ops, 2));
    EXPECT_EQ(1, SemCtl(sem.id(), 0, GETVAL, 0));
    EXPECT_EQ(8, SemCtl(sem.id(), 1, GETVAL, 0));
}

TEST(SysVSem, RepeatedSemOperationsObservePrecedingOperations) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    struct sembuf ops[2] = {{0, 1, 0}, {0, -1, 0}};
    struct timespec timeout = {0, 100 * 1000 * 1000};
    ASSERT_EQ(0, SemTimedOp(sem.id(), ops, 2, &timeout))
        << "the decrement must observe the preceding increment";
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0));
}

TEST(SysVSem, RepeatedSemNowaitFailureRollsBackWholeGroup) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 1));

    struct sembuf ops[2] = {{0, -1, 0}, {0, -1, IPC_NOWAIT}};
    errno = 0;
    EXPECT_EQ(-1, SemOp(sem.id(), ops, 2));
    EXPECT_EQ(EAGAIN, errno);
    EXPECT_EQ(1, SemCtl(sem.id(), 0, GETVAL, 0)) << "failed groups must not commit a prefix";
}

TEST(SysVSem, CumulativeOverflowRollsBackWholeGroup) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 32766));

    struct sembuf ops[2] = {{0, 1, 0}, {0, 1, 0}};
    errno = 0;
    EXPECT_EQ(-1, SemOp(sem.id(), ops, 2));
    EXPECT_EQ(ERANGE, errno);
    EXPECT_EQ(32766, SemCtl(sem.id(), 0, GETVAL, 0));
}

TEST(SysVSem, InvalidSemnum) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    struct sembuf op = {1, 1, 0};
    errno = 0;
    EXPECT_EQ(-1, SemOp(sem.id(), &op, 1));
    EXPECT_EQ(EFBIG, errno) << "out-of-range semnum";
}

TEST(SysVSem, InvalidSemid) {
    struct sembuf op = {0, 1, 0};
    errno = 0;
    EXPECT_EQ(-1, SemOp(0x3FFFFFFF, &op, 1));
    EXPECT_EQ(EINVAL, errno);

    errno = 0;
    EXPECT_EQ(-1, SemCtl(0x3FFFFFFF, 0, GETVAL, 0));
    EXPECT_EQ(EINVAL, errno);
}

// ============ SEM_UNDO ABI conformance ============
//
// Every case starts with an immediately-completing undo operation.  Until the
// kernel gate is enabled this is deliberately the first assertion to fail with
// ENOSYS, rather than allowing a later queue rendezvous to time out.
void RequireSemUndoAvailable(const SemSet& sem) {
    struct sembuf probe = {0, 1, SEM_UNDO};
    ASSERT_EQ(0, SemOp(sem.id(), &probe, 1))
        << "SEM_UNDO must be published only after all lifecycle prerequisites; errno=" << errno;
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 0)) << "SETVAL must clear probe debt";
}

TEST(SysVSem, SemUndoBasicAccumulationAndSign) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    RequireSemUndoAvailable(sem);

    int ready[2];
    int release[2];
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(release));
    FdGuard ready_read(ready[0]);
    FdGuard ready_write(ready[1]);
    FdGuard release_read(release[0]);
    FdGuard release_write(release[1]);
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        ready_read.Close();
        release_write.Close();
        struct sembuf ops[] = {{0, 3, SEM_UNDO}, {0, -1, SEM_UNDO}};
        const int result = SemOp(sem.id(), ops, 2);
        const int saved_errno = errno;
        if (!WriteExact(ready_write.get(), &result, sizeof(result)) ||
            !WriteExact(ready_write.get(), &saved_errno, sizeof(saved_errno))) {
            _exit(10);
        }
        char token;
        _exit(result == 0 && ReadExact(release_read.get(), &token, sizeof(token)) ? 0 : 11);
    }
    ready_write.Close();
    release_read.Close();
    int result;
    int child_errno;
    ASSERT_TRUE(ReadExact(ready_read.get(), &result, sizeof(result)));
    ASSERT_TRUE(ReadExact(ready_read.get(), &child_errno, sizeof(child_errno)));
    ASSERT_EQ(0, result) << "child SEM_UNDO ops failed: errno=" << child_errno;
    EXPECT_EQ(2, SemCtl(sem.id(), 0, GETVAL, 0));
    const char token = 1;
    ASSERT_TRUE(WriteExact(release_write.get(), &token, sizeof(token)));
    WaitChildOk(child);
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0));

    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 3));
    child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        _exit(SemUndoOpMustSucceed(sem.id(), 0, -2) ? 0 : 12);
    }
    WaitChildOk(child);
    EXPECT_EQ(3, SemCtl(sem.id(), 0, GETVAL, 0));
}

TEST(SysVSem, SemUndoArrayOrderAndRollback) {
    SemSet sem(2, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    RequireSemUndoAvailable(sem);

    struct sembuf blocked[] = {{0, 1, SEM_UNDO}, {1, -1, SEM_UNDO | IPC_NOWAIT}};
    errno = 0;
    EXPECT_EQ(-1, SemOp(sem.id(), blocked, 2));
    EXPECT_EQ(EAGAIN, errno);
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0));

    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 32767));
    struct sembuf overflow[] = {{0, -1, SEM_UNDO}, {0, 1, SEM_UNDO}, {0, 1, SEM_UNDO}};
    errno = 0;
    EXPECT_EQ(-1, SemOp(sem.id(), overflow, 3));
    EXPECT_EQ(ERANGE, errno);
    EXPECT_EQ(32767, SemCtl(sem.id(), 0, GETVAL, 0));
}

TEST(SysVSem, SemUndoAdjustmentBounds) {
    SemSet negative(1, IPC_CREAT | 0600);
    SemSet positive(1, IPC_CREAT | 0600);
    ASSERT_TRUE(negative.valid());
    ASSERT_TRUE(positive.valid());
    RequireSemUndoAvailable(negative);
    RequireSemUndoAvailable(positive);

    // semadj is the inverse of sem_op. One max-sized undo operation plus a
    // one-unit undo operation reaches -32768 while the plain reverse keeps
    // semval at zero. The following operation must exceed the signed bound.
    pid_t child = fork();
    ASSERT_GE(child, 0);
    ChildGuard negative_child(child);
    if (child == 0) {
        struct sembuf to_minus_32767[] = {{0, 32767, SEM_UNDO}, {0, -32767, 0}};
        struct sembuf reach_minus_32768[] = {{0, 1, SEM_UNDO}, {0, -1, 0}};
        struct sembuf below_min[] = {{0, 1, SEM_UNDO}, {0, -1, 0}};
        if (SemOp(negative.id(), to_minus_32767, 2) != 0 ||
            SemOp(negative.id(), reach_minus_32768, 2) != 0) {
            _exit(40);
        }
        errno = 0;
        _exit(SemOp(negative.id(), below_min, 2) == -1 && errno == ERANGE ? 0 : 41);
    }
    WaitChildOk(&negative_child);
    EXPECT_EQ(0, SemCtl(negative.id(), 0, GETVAL, 0));

    // Start at SEMVMX. A max-sized negative undo plus a plain reverse reaches
    // +32767 semadj without blocking; the following IPC_NOWAIT decrement must
    // fail for semadj overflow rather than queueing.
    ASSERT_EQ(0, SemCtl(positive.id(), 0, SETVAL, 32767));
    child = fork();
    ASSERT_GE(child, 0);
    ChildGuard positive_child(child);
    if (child == 0) {
        struct sembuf to_plus_32767[] = {{0, -32767, SEM_UNDO | IPC_NOWAIT}, {0, 32767, 0}};
        struct sembuf above_max[] = {{0, -1, SEM_UNDO | IPC_NOWAIT}, {0, 1, 0}};
        if (SemOp(positive.id(), to_plus_32767, 2) != 0) {
            _exit(42);
        }
        errno = 0;
        _exit(SemOp(positive.id(), above_max, 2) == -1 && errno == ERANGE ? 0 : 43);
    }
    WaitChildOk(&positive_child);
    EXPECT_EQ(32767, SemCtl(positive.id(), 0, GETVAL, 0));
}

TEST(SysVSem, SemUndoQueueCapturedOwner) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    RequireSemUndoAvailable(sem);

    int ready[2];
    int release[2];
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(release));
    pid_t owner = fork();
    ASSERT_GE(owner, 0);
    if (owner == 0) {
        close(ready[0]);
        close(release[1]);
        struct sembuf dec = {0, -1, SEM_UNDO};
        int result = SemOp(sem.id(), &dec, 1);
        if (!WriteExact(ready[1], &result, sizeof(result))) {
            _exit(20);
        }
        char token;
        _exit(result == 0 && ReadExact(release[0], &token, sizeof(token)) ? 0 : 21);
    }
    FdGuard ready_read(ready[0]);
    FdGuard ready_write(ready[1]);
    FdGuard release_read(release[0]);
    FdGuard release_write(release[1]);
    ready_write.Close();
    release_read.Close();
    ASSERT_TRUE(WaitForNcnt(sem.id(), 0, 1));
    pid_t waker = fork();
    ASSERT_GE(waker, 0);
    if (waker == 0) {
        struct sembuf inc = {0, 1, 0};
        _exit(SemOp(sem.id(), &inc, 1) == 0 ? 0 : 22);
    }
    WaitChildOk(waker);
    int result;
    ASSERT_TRUE(ReadExact(ready_read.get(), &result, sizeof(result)));
    ASSERT_EQ(0, result);
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0)) << "ordinary waker exit must not replay A debt";
    char token = 1;
    ASSERT_TRUE(WriteExact(release_write.get(), &token, sizeof(token)));
    WaitChildOk(owner);
    EXPECT_EQ(1, SemCtl(sem.id(), 0, GETVAL, 0));
}

TEST(SysVSem, SemUndoUncommittedPaths) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    RequireSemUndoAvailable(sem);

    struct sembuf nowait = {0, -1, SEM_UNDO | IPC_NOWAIT};
    errno = 0;
    EXPECT_EQ(-1, SemOp(sem.id(), &nowait, 1));
    EXPECT_EQ(EAGAIN, errno);
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0));

    // A real blocking semtimedop must time out and leave no queued debt.
    struct sembuf timed = {0, -1, SEM_UNDO};
    const struct timespec timeout = {0, 20 * 1000 * 1000};
    errno = 0;
    EXPECT_EQ(-1, SemTimedOp(sem.id(), &timed, 1, &timeout));
    EXPECT_EQ(EAGAIN, errno);
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETNCNT, 0));

    int ready[2];
    ASSERT_EQ(0, pipe(ready));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    ChildGuard signal_child(child);
    if (child == 0) {
        close(ready[0]);
        struct sigaction sa = {};
        sa.sa_handler = [](int) {};
        sigemptyset(&sa.sa_mask);
        if (sigaction(SIGUSR1, &sa, nullptr) != 0) {
            _exit(30);
        }
        struct sembuf dec = {0, -1, SEM_UNDO};
        const char entered = 1;
        if (!WriteExact(ready[1], &entered, sizeof(entered))) {
            _exit(31);
        }
        int result = SemOp(sem.id(), &dec, 1);
        _exit(result == -1 && errno == EINTR ? 0 : 32);
    }
    FdGuard ready_read(ready[0]);
    FdGuard ready_write(ready[1]);
    ready_write.Close();
    char entered;
    ASSERT_TRUE(ReadExact(ready_read.get(), &entered, sizeof(entered)));
    ASSERT_TRUE(WaitForNcnt(sem.id(), 0, 1));
    ASSERT_EQ(0, kill(signal_child.pid(), SIGUSR1));
    WaitChildOk(&signal_child);
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0));

    // RMID must wake an uncommitted SEM_UNDO waiter with EIDRM and cannot replay debt.
    SemSet removed(1, IPC_CREAT | 0600);
    ASSERT_TRUE(removed.valid());
    int rmid_ready[2];
    ASSERT_EQ(0, pipe(rmid_ready));
    child = fork();
    ASSERT_GE(child, 0);
    ChildGuard rmid_child(child);
    if (child == 0) {
        close(rmid_ready[0]);
        struct sembuf dec = {0, -1, SEM_UNDO};
        const char queued = 1;
        if (!WriteExact(rmid_ready[1], &queued, sizeof(queued))) {
            _exit(33);
        }
        const int result = SemOp(removed.id(), &dec, 1);
        _exit(result == -1 && errno == EIDRM ? 0 : 34);
    }
    FdGuard rmid_ready_read(rmid_ready[0]);
    FdGuard rmid_ready_write(rmid_ready[1]);
    rmid_ready_write.Close();
    ASSERT_TRUE(ReadExact(rmid_ready_read.get(), &entered, sizeof(entered)));
    ASSERT_TRUE(WaitForNcnt(removed.id(), 0, 1));
    ASSERT_EQ(0, SemCtl(removed.id(), 0, IPC_RMID, 0));
    removed.release();
    WaitChildOk(&rmid_child);
}

TEST(SysVSem, SemUndoSetvalSetallClearDebt) {
    SemSet sem(2, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    RequireSemUndoAvailable(sem);

    int ready[2];
    int release[2];
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(release));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(ready[0]);
        close(release[1]);
        struct sembuf ops[] = {{0, 1, SEM_UNDO}, {1, 2, SEM_UNDO}};
        const int result = SemOp(sem.id(), ops, 2);
        if (!WriteExact(ready[1], &result, sizeof(result))) {
            _exit(40);
        }
        char token;
        _exit(result == 0 && ReadExact(release[0], &token, sizeof(token)) ? 0 : 41);
    }
    FdGuard ready_read(ready[0]);
    FdGuard ready_write(ready[1]);
    FdGuard release_read(release[0]);
    FdGuard release_write(release[1]);
    ready_write.Close();
    release_read.Close();
    int result;
    ASSERT_TRUE(ReadExact(ready_read.get(), &result, sizeof(result)));
    ASSERT_EQ(0, result);
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 7));
    char token = 1;
    ASSERT_TRUE(WriteExact(release_write.get(), &token, sizeof(token)));
    WaitChildOk(child);
    EXPECT_EQ(7, SemCtl(sem.id(), 0, GETVAL, 0)) << "SETVAL clears only target debt";
    EXPECT_EQ(0, SemCtl(sem.id(), 1, GETVAL, 0)) << "un-cleared sem debt replays";

    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(release));
    child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(ready[0]);
        close(release[1]);
        struct sembuf ops[] = {{0, 1, SEM_UNDO}, {1, 1, SEM_UNDO}};
        const int child_result = SemOp(sem.id(), ops, 2);
        if (!WriteExact(ready[1], &child_result, sizeof(child_result))) {
            _exit(42);
        }
        char child_token;
        _exit(child_result == 0 && ReadExact(release[0], &child_token, sizeof(child_token)) ? 0 : 43);
    }
    FdGuard all_ready_read(ready[0]);
    FdGuard all_ready_write(ready[1]);
    FdGuard all_release_read(release[0]);
    FdGuard all_release_write(release[1]);
    all_ready_write.Close();
    all_release_read.Close();
    ASSERT_TRUE(ReadExact(all_ready_read.get(), &result, sizeof(result)));
    ASSERT_EQ(0, result);
    unsigned short vals[] = {4, 5};
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETALL, reinterpret_cast<unsigned long>(vals)));
    token = 1;
    ASSERT_TRUE(WriteExact(all_release_write.get(), &token, sizeof(token)));
    WaitChildOk(child);
    EXPECT_EQ(4, SemCtl(sem.id(), 0, GETVAL, 0)) << "SETALL clears first adjustment";
    EXPECT_EQ(5, SemCtl(sem.id(), 1, GETVAL, 0)) << "SETALL clears complete set slice";
    // SETVAL wakes a queued UNDO request. Its undo debt is committed only when
    // dequeued, so it must still replay when the waiter exits.
    SemSet queued(1, IPC_CREAT | 0600);
    ASSERT_TRUE(queued.valid());
    int queued_ready[2];
    int queued_release[2];
    ASSERT_EQ(0, pipe(queued_ready));
    ASSERT_EQ(0, pipe(queued_release));
    child = fork();
    ASSERT_GE(child, 0);
    ChildGuard queued_child(child);
    if (child == 0) {
        close(queued_ready[0]);
        close(queued_release[1]);
        struct sembuf dec = {0, -1, SEM_UNDO};
        const char entered = 1;
        if (!WriteExact(queued_ready[1], &entered, sizeof(entered)) || SemOp(queued.id(), &dec, 1) != 0) {
            _exit(44);
        }
        char release_token;
        _exit(ReadExact(queued_release[0], &release_token, sizeof(release_token)) ? 0 : 45);
    }
    FdGuard queued_ready_read(queued_ready[0]);
    FdGuard queued_ready_write(queued_ready[1]);
    FdGuard queued_release_read(queued_release[0]);
    FdGuard queued_release_write(queued_release[1]);
    queued_ready_write.Close();
    queued_release_read.Close();
    char queued_token;
    ASSERT_TRUE(ReadExact(queued_ready_read.get(), &queued_token, sizeof(queued_token)));
    ASSERT_TRUE(WaitForNcnt(queued.id(), 0, 1));
    ASSERT_EQ(0, SemCtl(queued.id(), 0, SETVAL, 1));
    EXPECT_EQ(0, SemCtl(queued.id(), 0, GETVAL, 0));
    queued_token = 1;
    ASSERT_TRUE(WriteExact(queued_release_write.get(), &queued_token, sizeof(queued_token)));
    WaitChildOk(&queued_child);
    EXPECT_EQ(1, SemCtl(queued.id(), 0, GETVAL, 0))
        << "debt committed after SETVAL wake must replay at owner exit";
}

TEST(SysVSem, SemUndoRmidDiscardsDebt) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    RequireSemUndoAvailable(sem);
    int ready[2];
    int release[2];
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(release));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(ready[0]);
        close(release[1]);
        const int result = SemUndoOpMustSucceed(sem.id(), 0, 1) ? 0 : -1;
        if (!WriteExact(ready[1], &result, sizeof(result))) {
            _exit(50);
        }
        char token;
        _exit(result == 0 && ReadExact(release[0], &token, sizeof(token)) ? 0 : 51);
    }
    FdGuard ready_read(ready[0]);
    FdGuard ready_write(ready[1]);
    FdGuard release_read(release[0]);
    FdGuard release_write(release[1]);
    ready_write.Close();
    release_read.Close();
    int result;
    ASSERT_TRUE(ReadExact(ready_read.get(), &result, sizeof(result)));
    ASSERT_EQ(0, result);
    ASSERT_EQ(0, SemCtl(sem.id(), 0, IPC_RMID, 0));
    sem.release();
    char token = 1;
    ASSERT_TRUE(WriteExact(release_write.get(), &token, sizeof(token)));
    WaitChildOk(child);
}

struct CloneUndoArgs {
    int semid;
    int ready_fd;
    int release_fd;
};

int CloneSysvsemUndoChild(void* opaque) {
    CloneUndoArgs* args = static_cast<CloneUndoArgs*>(opaque);
    if (!SemUndoOpMustSucceed(args->semid, 0, 1)) {
        return 1;
    }
    if (args->ready_fd >= 0) {
        const char ready = 1;
        if (!WriteExact(args->ready_fd, &ready, sizeof(ready))) {
            return 2;
        }
    }
    if (args->release_fd >= 0) {
        char release;
        if (!ReadExact(args->release_fd, &release, sizeof(release))) {
            return 3;
        }
    }
    return 0;
}

TEST(SysVSem, SemUndoForkAndCloneOwners) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    RequireSemUndoAvailable(sem);

    pid_t supervisor = fork();
    ASSERT_GE(supervisor, 0);
    ChildGuard supervisor_guard(supervisor);
    if (supervisor == 0) {
        if (!SemUndoOpMustSucceed(sem.id(), 0, 1)) {
            _exit(60);
        }
        pid_t ordinary = fork();
        if (ordinary < 0) {
            _exit(61);
        }
        if (ordinary == 0) {
            _exit(0);
        }
        int status;
        if (waitpid(ordinary, &status, 0) != ordinary || !WIFEXITED(status) || WEXITSTATUS(status) ||
            SemCtl(sem.id(), 0, GETVAL, 0) != 1) {
            _exit(62);
        }

        alignas(16) char shared_stack[16384];
        CloneUndoArgs shared_args = {sem.id(), -1, -1};
        pid_t shared = clone(CloneSysvsemUndoChild, shared_stack + sizeof(shared_stack),
                             CLONE_SYSVSEM | SIGCHLD, &shared_args);
        if (shared < 0 || waitpid(shared, &status, 0) != shared || !WIFEXITED(status) || WEXITSTATUS(status) ||
            SemCtl(sem.id(), 0, GETVAL, 0) != 2) {
            _exit(63);
        }

        int ready[2];
        int release[2];
        if (pipe(ready) != 0 || pipe(release) != 0) {
            _exit(64);
        }
        alignas(16) char thread_stack[16384];
        CloneUndoArgs thread_args = {sem.id(), ready[1], release[0]};
        const int thread_flags = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD;
        if (clone(CloneSysvsemUndoChild, thread_stack + sizeof(thread_stack), thread_flags, &thread_args) < 0) {
            _exit(65);
        }
        close(ready[1]);
        close(release[0]);
        char token;
        if (!ReadExact(ready[0], &token, sizeof(token)) || SemCtl(sem.id(), 0, GETVAL, 0) != 3) {
            _exit(66);
        }
        token = 1;
        if (!WriteExact(release[1], &token, sizeof(token))) {
            _exit(67);
        }
        close(release[1]);
        // EOF is the explicit thread-exit handshake. The thread is the last
        // writer, so EOF and the value together prove independent replay.
        if (ReadExact(ready[0], &token, sizeof(token)) || SemCtl(sem.id(), 0, GETVAL, 0) != 2) {
            _exit(68);
        }
        close(ready[0]);
        _exit(0);
    }
    WaitChildOk(&supervisor_guard);
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0))
        << "only final CLONE_SYSVSEM owner replays its shared debt";

    alignas(16) char invalid_stack[16384];
    CloneUndoArgs invalid_args = {sem.id(), -1, -1};
    errno = 0;
    EXPECT_EQ(-1, clone(CloneSysvsemUndoChild, invalid_stack + sizeof(invalid_stack),
                        CLONE_NEWIPC | CLONE_SYSVSEM | SIGCHLD, &invalid_args));
    EXPECT_EQ(EINVAL, errno);
}

const char* g_program_path = nullptr;

TEST(SysVSem, SemUndoExecPreservesAttachment) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    RequireSemUndoAvailable(sem);
    ASSERT_NE(nullptr, g_program_path);
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (!SemUndoOpMustSucceed(sem.id(), 0, 1)) {
            _exit(70);
        }
        char* const argv[] = {const_cast<char*>(g_program_path), const_cast<char*>("--sem-undo-exec-helper"), nullptr};
        execv(g_program_path, argv);
        _exit(71);
    }
    WaitChildOk(child);
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0));

    child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (!SemUndoOpMustSucceed(sem.id(), 0, 1)) {
            _exit(72);
        }
        char* const argv[] = {const_cast<char*>("/missing-sem-undo-helper"), nullptr};
        execv(argv[0], argv);
        _exit(errno == ENOENT ? 0 : 73);
    }
    WaitChildOk(child);
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0));
}

TEST(SysVSem, SemUndoExitClampAndWake) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    RequireSemUndoAvailable(sem);

    // Exit replay is saturating at SEMVMX: +1 debt meets a concurrently raised value.
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 32766));
    int ready[2];
    int release[2];
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(release));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    ChildGuard upper_child(child);
    if (child == 0) {
        close(ready[0]);
        close(release[1]);
        if (!SemUndoOpMustSucceed(sem.id(), 0, -1)) {
            _exit(80);
        }
        const char token = 1;
        if (!WriteExact(ready[1], &token, sizeof(token))) {
            _exit(81);
        }
        char release_token;
        _exit(ReadExact(release[0], &release_token, sizeof(release_token)) ? 0 : 82);
    }
    FdGuard upper_ready_read(ready[0]);
    FdGuard upper_ready_write(ready[1]);
    FdGuard upper_release_read(release[0]);
    FdGuard upper_release_write(release[1]);
    upper_ready_write.Close();
    upper_release_read.Close();
    char token;
    ASSERT_TRUE(ReadExact(upper_ready_read.get(), &token, sizeof(token)));
    struct sembuf raise_to_max = {0, 2, 0};
    ASSERT_EQ(0, SemOp(sem.id(), &raise_to_max, 1));
    ASSERT_TRUE(WriteExact(upper_release_write.get(), &token, sizeof(token)));
    WaitChildOk(&upper_child);
    EXPECT_EQ(32767, SemCtl(sem.id(), 0, GETVAL, 0));

    // Negative replay also saturates at zero: +1 debt meets a forced zero value.
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 1));
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(release));
    child = fork();
    ASSERT_GE(child, 0);
    ChildGuard lower_child(child);
    if (child == 0) {
        close(ready[0]);
        close(release[1]);
        if (!SemUndoOpMustSucceed(sem.id(), 0, 1)) {
            _exit(83);
        }
        const char child_token = 1;
        if (!WriteExact(ready[1], &child_token, sizeof(child_token))) {
            _exit(84);
        }
        char release_token;
        _exit(ReadExact(release[0], &release_token, sizeof(release_token)) ? 0 : 85);
    }
    FdGuard lower_ready_read(ready[0]);
    FdGuard lower_ready_write(ready[1]);
    FdGuard lower_release_read(release[0]);
    FdGuard lower_release_write(release[1]);
    lower_ready_write.Close();
    lower_release_read.Close();
    ASSERT_TRUE(ReadExact(lower_ready_read.get(), &token, sizeof(token)));
    struct sembuf lower_to_zero = {0, -2, 0};
    ASSERT_EQ(0, SemOp(sem.id(), &lower_to_zero, 1));
    ASSERT_TRUE(WriteExact(lower_release_write.get(), &token, sizeof(token)));
    WaitChildOk(&lower_child);
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0));

    // Final-owner replay must wake a zero waiter, validated by GETZCNT rather than timing.
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 0));
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(release));
    child = fork();
    ASSERT_GE(child, 0);
    ChildGuard wake_owner(child);
    if (child == 0) {
        close(ready[0]);
        close(release[1]);
        if (!SemUndoOpMustSucceed(sem.id(), 0, 1)) {
            _exit(86);
        }
        const char child_token = 1;
        if (!WriteExact(ready[1], &child_token, sizeof(child_token))) {
            _exit(87);
        }
        char release_token;
        _exit(ReadExact(release[0], &release_token, sizeof(release_token)) ? 0 : 88);
    }
    FdGuard wake_ready_read(ready[0]);
    FdGuard wake_ready_write(ready[1]);
    FdGuard wake_release_read(release[0]);
    FdGuard wake_release_write(release[1]);
    wake_ready_write.Close();
    wake_release_read.Close();
    ASSERT_TRUE(ReadExact(wake_ready_read.get(), &token, sizeof(token)));
    pid_t waiter = fork();
    ASSERT_GE(waiter, 0);
    ChildGuard wake_waiter(waiter);
    if (waiter == 0) {
        struct sembuf zero = {0, 0, 0};
        _exit(SemOp(sem.id(), &zero, 1) == 0 ? 0 : 89);
    }
    ASSERT_TRUE(WaitForZcnt(sem.id(), 0, 1));
    ASSERT_TRUE(WriteExact(wake_release_write.get(), &token, sizeof(token)));
    WaitChildOk(&wake_owner);
    WaitChildOk(&wake_waiter);
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0));
}

TEST(SysVSem, SemUndoUnshareSysvsem) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    RequireSemUndoAvailable(sem);

    int phase[2];
    int supervisor_release[2];
    int old_owner_release[2];
    int old_owner_ready[2];
    ASSERT_EQ(0, pipe(phase));
    ASSERT_EQ(0, pipe(supervisor_release));
    ASSERT_EQ(0, pipe(old_owner_release));
    ASSERT_EQ(0, pipe(old_owner_ready));
    pid_t supervisor = fork();
    ASSERT_GE(supervisor, 0);
    ChildGuard supervisor_guard(supervisor);
    if (supervisor == 0) {
        close(phase[0]);
        close(supervisor_release[1]);
        close(old_owner_release[1]);
        if (!SemUndoOpMustSucceed(sem.id(), 0, 1)) {
            _exit(90);
        }
        alignas(16) char stack[16384];
        CloneUndoArgs args = {sem.id(), old_owner_ready[1], old_owner_release[0]};
        pid_t old_owner = clone(CloneSysvsemUndoChild, stack + sizeof(stack),
                                CLONE_SYSVSEM | SIGCHLD, &args);
        close(old_owner_ready[1]);
        char ready;
        if (old_owner < 0 || !ReadExact(old_owner_ready[0], &ready, sizeof(ready)) ||
            unshare(CLONE_SYSVSEM) != 0 ||
            !SemUndoOpMustSucceed(sem.id(), 0, 1)) {
            _exit(91);
        }
        const char detached = 1;
        if (!WriteExact(phase[1], &detached, sizeof(detached))) {
            _exit(92);
        }
        char release_supervisor;
        if (!ReadExact(supervisor_release[0], &release_supervisor, sizeof(release_supervisor))) {
            _exit(93);
        }
        // Do not wait for old_owner here: it is intentionally held by the parent
        // until this process exits and replays the new group's debt.
        _exit(0);
    }
    FdGuard phase_read(phase[0]);
    FdGuard phase_write(phase[1]);
    FdGuard supervisor_read(supervisor_release[0]);
    FdGuard supervisor_write(supervisor_release[1]);
    FdGuard old_owner_read(old_owner_release[0]);
    FdGuard old_owner_write(old_owner_release[1]);
    phase_write.Close();
    supervisor_read.Close();
    old_owner_read.Close();
    close(old_owner_ready[0]);
    close(old_owner_ready[1]);
    char token;
    ASSERT_TRUE(ReadExact(phase_read.get(), &token, sizeof(token)));
    EXPECT_EQ(3, SemCtl(sem.id(), 0, GETVAL, 0))
        << "new group debt must coexist with the still-live old shared group";
    ASSERT_TRUE(WriteExact(supervisor_write.get(), &token, sizeof(token)));
    WaitChildOk(&supervisor_guard);
    EXPECT_EQ(2, SemCtl(sem.id(), 0, GETVAL, 0))
        << "unshared owner exit replays only its new group debt";
    ASSERT_TRUE(WriteExact(old_owner_write.get(), &token, sizeof(token)));
    ASSERT_TRUE(WaitForSemValue(sem.id(), 0, 0));
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0))
        << "last old shared owner replays old-group debt after its release";
}

struct NamespaceUndoReport {
    int setns_result;
    int setns_errno;
    int value_before_exit;
    int inaccessible_sem_errno;
    int precondition_errno;
};

int RunNamespaceSetnsChild(int semid, int setns_fd, int flags, int report_fd) {
    NamespaceUndoReport report = {-1, 0, -1, 0, 0};
    if (!SemUndoOpMustSucceed(semid, 0, 1)) {
        return 101;
    }
    errno = 0;
    report.setns_result = static_cast<int>(syscall(SYS_setns, setns_fd, flags));
    report.setns_errno = errno;
    report.value_before_exit = SemCtl(semid, 0, GETVAL, 0);
    return WriteExact(report_fd, &report, sizeof(report)) ? 0 : 102;
}

bool ReadNamespaceReportAndReap(int report_fd, ChildGuard* child, NamespaceUndoReport* report) {
    if (!ReadExact(report_fd, report, sizeof(*report))) {
        return false;
    }
    WaitChildOk(child);
    return true;
}

TEST(SysVSem, SemtimedopCopiesNonNullTimeoutBeforeNsopsValidation) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    struct sembuf op = {0, 0, 0};
    const struct timespec* invalid_timeout = reinterpret_cast<const struct timespec*>(
        static_cast<uintptr_t>(1));

    errno = 0;
    EXPECT_EQ(-1, SemTimedOp(sem.id(), &op, 0, invalid_timeout));
    EXPECT_EQ(EFAULT, errno)
        << "non-null timeout must be copied before nsops == 0 is rejected";
}

TEST(SysVSem, RawSyscallIntArgumentsUseLow32Bits) {
    static_assert(sizeof(unsigned long) == 8, "64-bit syscall ABI test");
    const unsigned long high = 1UL << 32;
    int id = static_cast<int>(syscall(SYS_semget, IPC_PRIVATE, high | 1, IPC_CREAT | 0600));
    ASSERT_GE(id, 0);
    SemSet sem(id, true);
    struct sembuf zero = {0, 0, 0};
    struct timespec timeout = {0, 0};
    EXPECT_EQ(0, syscall(SYS_semop, high | id, &zero, high | 1));
    EXPECT_EQ(0, syscall(SYS_semtimedop, high | id, &zero, high | 1, &timeout));
    EXPECT_EQ(0, syscall(SYS_semctl, high | id, high, high | GETVAL, 0UL));
    EXPECT_EQ(0, syscall(SYS_semctl, high | id, high, high | SETVAL, high | 7));
    EXPECT_EQ(7, SemCtl(id, 0, GETVAL, 0));
    unsigned short value = 0;
    EXPECT_EQ(0, syscall(SYS_semctl, high | id, ~0UL, high | GETALL, &value));
    EXPECT_EQ(7, value) << "GETALL ignores even negative semnum and preserves pointer width";
    struct seminfo info = {};
    EXPECT_GE(syscall(SYS_semctl, 0UL, ~0UL, high | IPC_INFO, &info), 0);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_semctl, 0xffffffffUL, 0UL, IPC_INFO, &info));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_semget, IPC_PRIVATE, high | 0xffffffffUL, IPC_CREAT | 0600));
    EXPECT_EQ(EINVAL, errno);
}

TEST(SysVSem, RawSyscallCombinedErrorOrder) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    struct sembuf zero = {0, 0, 0};
    struct timespec invalid_value = {0, -1};
    auto bad_ops = reinterpret_cast<struct sembuf*>(static_cast<uintptr_t>(1));
    const unsigned long negative_id = 0xffffffffUL;
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_semtimedop, sem.id(), &zero, 501UL, &invalid_value));
    EXPECT_EQ(E2BIG, errno);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_semtimedop, sem.id(), bad_ops, 1UL, &invalid_value));
    EXPECT_EQ(EFAULT, errno);
    for (long nr : {static_cast<long>(SYS_semop), static_cast<long>(SYS_semtimedop)}) {
        errno = 0;
        EXPECT_EQ(-1, syscall(nr, negative_id, &zero, 501UL, nullptr));
        EXPECT_EQ(E2BIG, errno);
        errno = 0;
        EXPECT_EQ(-1, syscall(nr, negative_id, bad_ops, 1UL, nullptr));
        EXPECT_EQ(EFAULT, errno);
        errno = 0;
        EXPECT_EQ(-1, syscall(nr, negative_id, &zero, 1UL, nullptr));
        EXPECT_EQ(EINVAL, errno);
    }
}

TEST(SysVSem, SemUndoIpcNamespaceAndErrnos) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    RequireSemUndoAvailable(sem);

    struct sembuf invalid_semnum = {1, 1, SEM_UNDO};
    errno = 0;
    EXPECT_EQ(-1, SemOp(sem.id(), &invalid_semnum, 1));
    EXPECT_EQ(EFBIG, errno);
    errno = 0;
    EXPECT_EQ(-1, SemOp(sem.id(), nullptr, 1));
    EXPECT_EQ(EFAULT, errno);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_semop, sem.id(), &invalid_semnum, 501));
    EXPECT_EQ(E2BIG, errno);

    for (bool valid_fd : {false, true}) {
        int report_pipe[2];
        ASSERT_EQ(0, pipe(report_pipe));
        pid_t child = fork();
        ASSERT_GE(child, 0);
        ChildGuard invalid_child(child);
        if (child == 0) {
            close(report_pipe[0]);
            _exit(RunNamespaceSetnsChild(sem.id(), valid_fd ? report_pipe[1] : -1,
                                        CLONE_NEWIPC | CLONE_SYSVSEM, report_pipe[1]));
        }
        FdGuard report_read(report_pipe[0]);
        FdGuard report_write(report_pipe[1]);
        report_write.Close();
        NamespaceUndoReport report = {};
        ASSERT_TRUE(ReadNamespaceReportAndReap(report_read.get(), &invalid_child, &report));
        EXPECT_EQ(-1, report.setns_result);
        EXPECT_EQ(valid_fd ? EINVAL : EBADF, report.setns_errno);
        EXPECT_EQ(1, report.value_before_exit);
        EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0))
            << "failed setns prepare must preserve old debt until child exit";
    }
}

TEST(SysVSem, SemUndoNamespaceFdDetachesAttachment) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    RequireSemUndoAvailable(sem);

    int nsfd = open("/proc/self/ns/ipc", O_RDONLY | O_CLOEXEC);
    if (nsfd < 0) {
        GTEST_SKIP() << "proc namespace-fd IPC setns unavailable (errno=" << errno
                     << "): requires FilePrivateData::Namespace; see "
                        "kernel/src/process/namespace/setns.rs:150-155";
    }
    FdGuard namespace_fd(nsfd);
    int report_pipe[2];
    ASSERT_EQ(0, pipe(report_pipe));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    ChildGuard nsfd_child(child);
    if (child == 0) {
        close(report_pipe[0]);
        _exit(RunNamespaceSetnsChild(sem.id(), namespace_fd.get(), CLONE_NEWIPC, report_pipe[1]));
    }
    FdGuard report_read(report_pipe[0]);
    FdGuard report_write(report_pipe[1]);
    report_write.Close();
    NamespaceUndoReport report = {};
    ASSERT_TRUE(ReadNamespaceReportAndReap(report_read.get(), &nsfd_child, &report));
    if (report.setns_result == -1 && (report.setns_errno == EPERM || report.setns_errno == EACCES)) {
        GTEST_SKIP() << "IPC namespace-fd setns needs CAP_SYS_ADMIN in target user namespace";
    }
    ASSERT_EQ(0, report.setns_result) << "namespace-fd setns errno=" << report.setns_errno;
    EXPECT_EQ(0, report.value_before_exit)
        << "successful namespace-fd detach must replay and discard old attachment";
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0))
        << "namespace-fd detach must leave no old attachment for child exit";
}

TEST(SysVSem, SemUndoPidfdDetachesAttachment) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    RequireSemUndoAvailable(sem);

    int pidfd = static_cast<int>(syscall(SYS_pidfd_open, getpid(), 0));
    if (pidfd < 0) {
        GTEST_SKIP() << "pidfd_open unavailable errno=" << errno;
    }
    FdGuard pidfd_guard(pidfd);
    int report_pipe[2];
    ASSERT_EQ(0, pipe(report_pipe));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    ChildGuard pidfd_child(child);
    if (child == 0) {
        close(report_pipe[0]);
        _exit(RunNamespaceSetnsChild(sem.id(), pidfd_guard.get(), CLONE_NEWIPC, report_pipe[1]));
    }
    FdGuard report_read(report_pipe[0]);
    FdGuard report_write(report_pipe[1]);
    report_write.Close();
    NamespaceUndoReport report = {};
    ASSERT_TRUE(ReadNamespaceReportAndReap(report_read.get(), &pidfd_child, &report));
    if (report.setns_result == -1 && (report.setns_errno == EPERM || report.setns_errno == EACCES)) {
        GTEST_SKIP() << "pidfd IPC setns needs capability/permission in target user namespace";
    }
    ASSERT_EQ(0, report.setns_result) << "pidfd setns errno=" << report.setns_errno;
    EXPECT_EQ(0, report.value_before_exit)
        << "same-Arc pidfd setns must replay and discard old attachment";
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0))
        << "pidfd detach must leave no old attachment for child exit";
}

TEST(SysVSem, SemUndoCredentialDenied) {
    SemSet probe(1, IPC_CREAT | 0600);
    SemSet private_sem(1, IPC_CREAT | 0000);
    ASSERT_TRUE(probe.valid());
    ASSERT_TRUE(private_sem.valid());
    RequireSemUndoAvailable(probe);

    int permission_pipe[2];
    ASSERT_EQ(0, pipe(permission_pipe));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    ChildGuard permission_child(child);
    if (child == 0) {
        close(permission_pipe[0]);
        NamespaceUndoReport permission = {-1, 0, -1, 0, 0};
        if (setgid(1000) != 0 || setuid(1000) != 0) {
            permission.precondition_errno = errno;
        } else {
            struct sembuf access = {0, 1, SEM_UNDO | IPC_NOWAIT};
            errno = 0;
            SemOp(private_sem.id(), &access, 1);
            permission.inaccessible_sem_errno = errno;
        }
        const bool wrote = WriteExact(permission_pipe[1], &permission, sizeof(permission));
        _exit(wrote ? 0 : 103);
    }
    FdGuard permission_read(permission_pipe[0]);
    FdGuard permission_write(permission_pipe[1]);
    permission_write.Close();
    NamespaceUndoReport report = {};
    ASSERT_TRUE(ReadNamespaceReportAndReap(permission_read.get(), &permission_child, &report));
    ASSERT_EQ(0, SemCtl(private_sem.id(), 0, IPC_RMID, 0));
    private_sem.release();
    if (report.precondition_errno != 0) {
        GTEST_SKIP() << "credential-drop EACCES precondition unavailable errno=" << report.precondition_errno;
    }
    EXPECT_EQ(EACCES, report.inaccessible_sem_errno);
}

// ============ blocking & wakeup ============

TEST(SysVSem, BlockingWakeup) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    const pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno;

    if (child == 0) {
        // The child blocks on -1 while val == 0.
        struct sembuf dec = {0, -1, 0};
        if (SemOp(sem.id(), &dec, 1) != 0) {
            _exit(10);
        }
        _exit(0);
    }

    ASSERT_TRUE(WaitForWaiters(sem.id(), 0, 1)) << "child did not block";

    struct sembuf inc = {0, 1, 0};
    ASSERT_EQ(0, SemOp(sem.id(), &inc, 1)) << "parent inc failed: errno=" << errno;
    WaitChildOk(child);
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0)) << "sem consumed by child";
}

TEST(SysVSem, BlockingWakeupZero) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 1));

    const pid_t child = fork();
    ASSERT_GE(child, 0);

    if (child == 0) {
        // The child waits for val to become zero.
        struct sembuf wait = {0, 0, 0};
        if (SemOp(sem.id(), &wait, 1) != 0) {
            _exit(10);
        }
        _exit(0);
    }

    ASSERT_TRUE(WaitForWaiters(sem.id(), 0, 1)) << "child did not block";
    EXPECT_EQ(1, SemCtl(sem.id(), 0, GETZCNT, 0)) << "GETZCNT must count zero-waiters";

    struct sembuf dec = {0, -1, 0};
    ASSERT_EQ(0, SemOp(sem.id(), &dec, 1));
    WaitChildOk(child);
}

TEST(SysVSem, GetNcntCountsBlocked) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    const pid_t child = fork();
    ASSERT_GE(child, 0);

    if (child == 0) {
        struct sembuf dec = {0, -1, 0};
        if (SemOp(sem.id(), &dec, 1) != 0) {
            _exit(10);
        }
        _exit(0);
    }

    ASSERT_TRUE(WaitForWaiters(sem.id(), 0, 1)) << "child did not block";
    EXPECT_EQ(1, SemCtl(sem.id(), 0, GETNCNT, 0)) << "GETNCNT must count blocked decrements";

    struct sembuf inc = {0, 1, 0};
    ASSERT_EQ(0, SemOp(sem.id(), &inc, 1));
    WaitChildOk(child);
}

TEST(SysVSem, OneWakeAllMultiWaiters) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    constexpr int kWaiters = 4;
    pid_t children[kWaiters];
    for (int i = 0; i < kWaiters; ++i) {
        children[i] = fork();
        ASSERT_GE(children[i], 0);
        if (children[i] == 0) {
            struct sembuf dec = {0, -1, 0};
            if (SemOp(sem.id(), &dec, 1) != 0) {
                _exit(10);
            }
            _exit(0);
        }
    }

    ASSERT_TRUE(WaitForWaiters(sem.id(), 0, kWaiters)) << "children did not block";
    EXPECT_EQ(kWaiters, SemCtl(sem.id(), 0, GETNCNT, 0));

    // A single +kWaiters operation wakes all waiters.
    struct sembuf inc = {0, kWaiters, 0};
    ASSERT_EQ(0, SemOp(sem.id(), &inc, 1)) << "parent inc failed: errno=" << errno;
    for (int i = 0; i < kWaiters; ++i) {
        WaitChildOk(children[i]);
    }
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0));
}

TEST(SysVSem, SemtimedopTimeout) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    struct sembuf dec = {0, -1, 0};
    struct timespec timeout = {0, 100 * 1000 * 1000};  // 100ms
    errno = 0;
    EXPECT_EQ(-1, SemTimedOp(sem.id(), &dec, 1, &timeout));
    EXPECT_EQ(EAGAIN, errno) << "timeout must return EAGAIN";
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETNCNT, 0)) << "timeout must remove its queue entry";

    struct sembuf inc = {0, 1, 0};
    ASSERT_EQ(0, SemOp(sem.id(), &inc, 1));
    EXPECT_EQ(1, SemCtl(sem.id(), 0, GETVAL, 0))
        << "a timed-out operation must not consume a future token";
}

TEST(SysVSem, SemtimedopZeroTimeoutNoBlock) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    struct sembuf dec = {0, -1, 0};
    struct timespec timeout = {0, 0};
    errno = 0;
    EXPECT_EQ(-1, SemTimedOp(sem.id(), &dec, 1, &timeout));
    EXPECT_EQ(EAGAIN, errno);
}

TEST(SysVSem, SemtimedopSucceedsWithinTimeout) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    struct sembuf dec = {0, -1, 0};
    struct timespec timeout = {2, 0};  // 2s
    const pid_t child = fork();
    ASSERT_GE(child, 0);

    if (child == 0) {
        if (SemTimedOp(sem.id(), &dec, 1, &timeout) != 0) {
            _exit(10);
        }
        _exit(0);
    }

    ASSERT_TRUE(WaitForWaiters(sem.id(), 0, 1)) << "child did not block";
    struct sembuf inc = {0, 1, 0};
    ASSERT_EQ(0, SemOp(sem.id(), &inc, 1));
    WaitChildOk(child);
}

TEST(SysVSem, SignalInterruptsBlockedSemop) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    const pid_t child_pid = fork();
    ASSERT_GE(child_pid, 0);
    ChildGuard child(child_pid);

    if (child_pid == 0) {
        struct sigaction sa = {};
        sa.sa_handler = [](int) {};
        sigemptyset(&sa.sa_mask);
        if (sigaction(SIGUSR1, &sa, nullptr) != 0) {
            _exit(13);
        }

        struct sembuf dec = {0, -1, 0};
        int ret = SemOp(sem.id(), &dec, 1);
        // An interrupted blocked operation must return EINTR.
        if (ret == 0) {
            _exit(11);
        }
        if (errno != EINTR) {
            _exit(12);
        }
        _exit(0);
    }

    ASSERT_TRUE(WaitForWaiters(sem.id(), 0, 1)) << "child did not block";
    ASSERT_EQ(0, kill(child.pid(), SIGUSR1)) << "kill failed: errno=" << errno;
    WaitChildOk(&child);
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETNCNT, 0)) << "EINTR must remove its queue entry";

    struct sembuf inc = {0, 1, 0};
    ASSERT_EQ(0, SemOp(sem.id(), &inc, 1));
    EXPECT_EQ(1, SemCtl(sem.id(), 0, GETVAL, 0))
        << "an interrupted operation must not consume a future token";
}

TEST(SysVSem, ConstWaitersCompleteBeforeAlteringWaiters) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 1));

    const pid_t altering_pid = fork();
    ASSERT_GE(altering_pid, 0);
    ChildGuard altering(altering_pid);
    if (altering_pid == 0) {
        struct sembuf ops[] = {{0, 0, 0}, {0, 1, 0}};
        _exit(SemOp(sem.id(), ops, 2) == 0 ? 0 : 11);
    }
    ASSERT_TRUE(WaitForZcnt(sem.id(), 0, 1));
    ASSERT_EQ(1, SemCtl(sem.id(), 0, GETZCNT, 0));

    const pid_t constant_pid = fork();
    ASSERT_GE(constant_pid, 0);
    ChildGuard constant(constant_pid);
    if (constant_pid == 0) {
        struct sembuf wait_zero = {0, 0, 0};
        _exit(SemOp(sem.id(), &wait_zero, 1) == 0 ? 0 : 12);
    }
    ASSERT_TRUE(WaitForZcnt(sem.id(), 0, 2));
    ASSERT_EQ(2, SemCtl(sem.id(), 0, GETZCNT, 0));

    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 0));
    const int zcnt_after_setval = SemCtl(sem.id(), 0, GETZCNT, 0);
    if (zcnt_after_setval != 0) {
        ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 0))
            << "release a waiter left behind by incorrect queue ordering";
    }
    EXPECT_EQ(0, zcnt_after_setval);
    WaitChildOk(&altering);
    WaitChildOk(&constant);
    EXPECT_EQ(1, SemCtl(sem.id(), 0, GETVAL, 0));
}

TEST(SysVSem, WakingSemopCompletesEligibleQueuedOperationsBeforeReturn) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 1));

    ScopedAffinity affinity;
    ASSERT_TRUE(affinity.PinToFirstCpu()) << "failed to pin test to one CPU: errno=" << errno;

    const pid_t first_pid = fork();
    ASSERT_GE(first_pid, 0);
    ChildGuard first(first_pid);
    if (first_pid == 0) {
        struct sembuf wait_zero = {0, 0, 0};
        _exit(SemOp(sem.id(), &wait_zero, 1) == 0 ? 0 : 11);
    }
    ASSERT_TRUE(WaitForWaiters(sem.id(), 0, 1));
    ASSERT_EQ(1, SemCtl(sem.id(), 0, GETZCNT, 0));

    int report_pipe[2];
    ASSERT_EQ(0, pipe(report_pipe));
    FdGuard report_read(report_pipe[0]);
    FdGuard report_write(report_pipe[1]);
    const pid_t child_pid = fork();
    ASSERT_GE(child_pid, 0);
    ChildGuard child(child_pid);

    struct QueueCompletionReport {
        int semop_result;
        int semop_errno;
        int value_after_semop;
        int ncnt_after_semop;
        int zcnt_after_semop;
    };

    if (child_pid == 0) {
        report_read.Close();
        QueueCompletionReport report = {-1, 0, -1, -1, -1};
        if (!WaitForWaiters(sem.id(), 0, 2)) {
            report.semop_errno = ETIMEDOUT;
            SemCtl(sem.id(), 0, SETVAL, 2);
        } else {
            struct sembuf inc = {0, 1, 0};
            report.semop_result = SemOp(sem.id(), &inc, 1);
            report.semop_errno = errno;
            report.value_after_semop = SemCtl(sem.id(), 0, GETVAL, 0);
            report.ncnt_after_semop = SemCtl(sem.id(), 0, GETNCNT, 0);
            report.zcnt_after_semop = SemCtl(sem.id(), 0, GETZCNT, 0);
        }
        const ssize_t written = write(report_write.get(), &report, sizeof(report));
        report_write.Close();
        _exit(written == static_cast<ssize_t>(sizeof(report)) ? 0 : 10);
    }

    report_write.Close();
    struct sembuf dec_two = {0, -2, 0};
    struct timespec queue_timeout = {7, 0};
    ASSERT_EQ(0, SemTimedOp(sem.id(), &dec_two, 1, &queue_timeout));

    QueueCompletionReport report = {};
    ssize_t received;
    do {
        received = read(report_read.get(), &report, sizeof(report));
    } while (received < 0 && errno == EINTR);
    report_read.Close();
    ASSERT_EQ(static_cast<ssize_t>(sizeof(report)), received);
    EXPECT_EQ(0, report.semop_result) << "waking semop failed: errno=" << report.semop_errno;
    EXPECT_EQ(0, report.value_after_semop)
        << "the eligible non-head decrement must commit before the waking semop returns";
    EXPECT_EQ(0, report.ncnt_after_semop);
    EXPECT_EQ(0, report.zcnt_after_semop)
        << "the zero waiter enabled by the decrement must complete in the same queue scan";
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 0));
    WaitChildOk(&child);
    WaitChildOk(&first);
    EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0));
}

TEST(SysVSem, RemoveWhileWaiting) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    const pid_t child = fork();
    ASSERT_GE(child, 0);

    if (child == 0) {
        struct sembuf dec = {0, -1, 0};
        int ret = SemOp(sem.id(), &dec, 1);
        if (ret == 0) {
            _exit(11);
        }
        if (errno != EIDRM) {
            _exit(12);
        }
        _exit(0);
    }

    ASSERT_TRUE(WaitForWaiters(sem.id(), 0, 1)) << "child did not block";
    ASSERT_EQ(0, SemCtl(sem.id(), 0, IPC_RMID, 0)) << "IPC_RMID failed: errno=" << errno;
    sem.release();
    WaitChildOk(child);
}

// ============ concurrency ============

TEST(SysVSem, WaitQueuesGrowForConstAndAlterOperations) {
    constexpr int kWaiters = 32;
    for (bool constant : {true, false}) {
        SemSet sem(1, IPC_CREAT | 0600);
        ASSERT_TRUE(sem.valid());
        ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, constant ? 1 : 0));
        std::vector<std::unique_ptr<ChildGuard>> children;
        for (int i = 0; i < kWaiters; ++i) {
            pid_t pid = fork();
            ASSERT_GE(pid, 0);
            if (pid == 0) {
                struct sembuf op = {0, static_cast<short>(constant ? 0 : -1), 0};
                _exit(SemOp(sem.id(), &op, 1) == 0 ? 0 : 111);
            }
            children.emplace_back(new ChildGuard(pid));
        }
        ASSERT_TRUE(constant ? WaitForZcnt(sem.id(), 0, kWaiters)
                             : WaitForNcnt(sem.id(), 0, kWaiters));
        ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, constant ? 0 : kWaiters));
        for (auto& child : children) {
            WaitChildOk(child.get());
        }
        EXPECT_EQ(0, SemCtl(sem.id(), 0, GETVAL, 0));
        EXPECT_EQ(0, SemCtl(sem.id(), 0, constant ? GETZCNT : GETNCNT, 0));
    }
}

TEST(SysVSem, RemoveWakesBothWaitQueuesInBulk) {
    constexpr int kPairs = 16;
    SemSet sem(2, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, 1));
    std::vector<std::unique_ptr<ChildGuard>> children;
    for (int i = 0; i < kPairs * 2; ++i) {
        const bool constant = i % 2 == 0;
        pid_t pid = fork();
        ASSERT_GE(pid, 0);
        if (pid == 0) {
            struct sembuf op = {static_cast<unsigned short>(constant ? 0 : 1),
                                static_cast<short>(constant ? 0 : -1), SEM_UNDO};
            struct timespec timeout = {10, 0};
            int result = SemTimedOp(sem.id(), &op, 1, &timeout);
            _exit(result == -1 && errno == EIDRM ? 0 : 115);
        }
        children.emplace_back(new ChildGuard(pid));
    }
    ASSERT_TRUE(WaitForZcnt(sem.id(), 0, kPairs));
    ASSERT_TRUE(WaitForNcnt(sem.id(), 1, kPairs));
    ASSERT_EQ(0, SemCtl(sem.id(), 0, IPC_RMID, 0));
    sem.release();
    for (auto& child : children) {
        WaitChildOk(child.get());
    }
}

TEST(SysVSem, SharedUndoGroupControlChangesStaySetLocal) {
    for (int cmd : {SETVAL, SETALL, IPC_RMID}) {
        SemSet target(1, IPC_CREAT | 0600);
        SemSet other(1, IPC_CREAT | 0600);
        ASSERT_TRUE(target.valid());
        ASSERT_TRUE(other.valid());
        int ready[2], release[2];
        ASSERT_EQ(0, pipe(ready));
        FdGuard ready_read(ready[0]), ready_write(ready[1]);
        ASSERT_EQ(0, pipe(release));
        FdGuard release_read(release[0]), release_write(release[1]);
        pid_t pid = fork();
        ASSERT_GE(pid, 0);
        if (pid == 0) {
            ready_read.Close();
            release_write.Close();
            char token = 1;
            if (!SemUndoOpMustSucceed(target.id(), 0, 1) ||
                !SemUndoOpMustSucceed(other.id(), 0, 1) ||
                !WriteExact(ready_write.get(), &token, 1) ||
                !ReadExact(release_read.get(), &token, 1)) {
                _exit(116);
            }
            if (cmd != IPC_RMID) {
                // Existing records must remain associated after a debt clear.
                if (!SemUndoOpMustSucceed(target.id(), 0, 1) ||
                    !WriteExact(ready_write.get(), &token, 1) ||
                    !ReadExact(release_read.get(), &token, 1)) {
                    _exit(117);
                }
            }
            _exit(0);
        }
        ChildGuard child(pid);
        ready_write.Close();
        release_read.Close();
        char token = 1;
        ASSERT_TRUE(ReadExact(ready_read.get(), &token, 1));
        unsigned short value = 9;
        const unsigned long arg = cmd == SETALL ? reinterpret_cast<unsigned long>(&value) : 9;
        ASSERT_EQ(0, SemCtl(target.id(), 0, cmd, arg));
        std::unique_ptr<SemSet> replacement;
        if (cmd == IPC_RMID) {
            target.release();
            replacement.reset(new SemSet(1, IPC_CREAT | 0600));
            ASSERT_TRUE(replacement->valid());
            ASSERT_EQ(0, SemCtl(replacement->id(), 0, SETVAL, 9));
        }
        ASSERT_TRUE(WriteExact(release_write.get(), &token, 1));
        if (cmd != IPC_RMID) {
            ASSERT_TRUE(ReadExact(ready_read.get(), &token, 1));
            EXPECT_EQ(10, SemCtl(target.id(), 0, GETVAL, 0));
            ASSERT_EQ(0, SemCtl(target.id(), 0, cmd, arg));
            ASSERT_TRUE(WriteExact(release_write.get(), &token, 1));
        }
        WaitChildOk(&child);
        EXPECT_EQ(0, SemCtl(other.id(), 0, GETVAL, 0)) << "unrelated debt must replay";
        EXPECT_EQ(9, SemCtl(replacement ? replacement->id() : target.id(), 0, GETVAL, 0))
            << "cleared or removed debt must not replay";
    }
}

TEST(SysVSem, IndependentUndoGroupsSteadyStateAndSetall) {
    constexpr int kWorkers = 32;
    std::vector<std::unique_ptr<SemSet>> sets;
    std::vector<std::unique_ptr<ChildGuard>> children;
    std::vector<std::unique_ptr<FdGuard>> ready_reads;
    std::vector<std::unique_ptr<FdGuard>> release_writes;
    for (int i = 0; i < kWorkers; ++i) {
        sets.emplace_back(new SemSet(1, IPC_CREAT | 0600));
        ASSERT_TRUE(sets.back()->valid());
        int ready[2], release[2];
        ASSERT_EQ(0, pipe(ready));
        ASSERT_EQ(0, pipe(release));
        pid_t pid = fork();
        ASSERT_GE(pid, 0);
        if (pid == 0) {
            close(ready[0]);
            close(release[1]);
            const int id = sets.back()->id();
            char token = 1;
            // Register every group before the parent starts the steady-state phase.
            if (!SemUndoOpMustSucceed(id, 0, 1) ||
                !WriteExact(ready[1], &token, 1) || !ReadExact(release[0], &token, 1)) {
                _exit(112);
            }
            for (int j = 0; j < 100; ++j) {
                if (!SemUndoOpMustSucceed(id, 0, 1) || !SemUndoOpMustSucceed(id, 0, -1)) {
                    _exit(113);
                }
            }
            if (!WriteExact(ready[1], &token, 1) || !ReadExact(release[0], &token, 1)) {
                _exit(114);
            }
            _exit(0);
        }
        children.emplace_back(new ChildGuard(pid));
        close(ready[1]);
        close(release[0]);
        ready_reads.emplace_back(new FdGuard(ready[0]));
        release_writes.emplace_back(new FdGuard(release[1]));
    }
    char token = 1;
    for (auto& ready : ready_reads) {
        ASSERT_TRUE(ReadExact(ready->get(), &token, 1));
    }
    for (auto& release : release_writes) {
        ASSERT_TRUE(WriteExact(release->get(), &token, 1));
    }
    for (auto& ready : ready_reads) {
        ASSERT_TRUE(ReadExact(ready->get(), &token, 1));
    }
    for (auto& sem : sets) {
        EXPECT_EQ(1, SemCtl(sem->id(), 0, GETVAL, 0));
        unsigned short value = 9;
        ASSERT_EQ(0, SemCtl(sem->id(), 0, SETALL, reinterpret_cast<unsigned long>(&value)));
    }
    for (auto& release : release_writes) {
        ASSERT_TRUE(WriteExact(release->get(), &token, 1));
    }
    for (auto& child : children) {
        WaitChildOk(child.get());
    }
    for (auto& sem : sets) {
        EXPECT_EQ(9, SemCtl(sem->id(), 0, GETVAL, 0)) << "SETALL clears each group's exit debt";
    }
}

TEST(SysVSem, UndoGroupChurnAcrossPersistentSets) {
    constexpr int kSets = 16;
    constexpr int kWorkers = 16;
    std::vector<std::unique_ptr<SemSet>> sets;
    for (int i = 0; i < kSets; ++i) {
        sets.emplace_back(new SemSet(1, IPC_CREAT | 0600));
        ASSERT_TRUE(sets.back()->valid());
    }
    // Keep every set alive across repeated empty -> shared -> empty cycles.
    for (int round = 0; round < 3; ++round) {
        int ready[2], release[2];
        ASSERT_EQ(0, pipe(ready));
        FdGuard ready_read(ready[0]), ready_write(ready[1]);
        ASSERT_EQ(0, pipe(release));
        FdGuard release_read(release[0]), release_write(release[1]);
        std::vector<std::unique_ptr<ChildGuard>> children;
        for (int worker = 0; worker < kWorkers; ++worker) {
            pid_t pid = fork();
            ASSERT_GE(pid, 0);
            if (pid == 0) {
                ready_read.Close();
                release_write.Close();
                for (auto& sem : sets) {
                    // This group has no published record for the set yet.
                    struct sembuf fail = {0, -32767, SEM_UNDO | IPC_NOWAIT};
                    if (SemOp(sem->id(), &fail, 1) != -1 || errno != EAGAIN) {
                        _exit(118);
                    }
                    struct sembuf timed = {0, -32767, SEM_UNDO};
                    struct timespec timeout = {0, 1000000};
                    if (SemTimedOp(sem->id(), &timed, 1, &timeout) != -1 || errno != EAGAIN ||
                        !SemUndoOpMustSucceed(sem->id(), 0, 1)) {
                        _exit(119);
                    }
                }
                char token = 1;
                _exit(WriteExact(ready_write.get(), &token, 1) &&
                              ReadExact(release_read.get(), &token, 1) ? 0 : 120);
            }
            children.emplace_back(new ChildGuard(pid));
        }
        ready_write.Close();
        release_read.Close();
        char token = 1;
        for (int i = 0; i < kWorkers; ++i) {
            ASSERT_TRUE(ReadExact(ready_read.get(), &token, 1));
        }
        for (auto& sem : sets) {
            EXPECT_EQ(kWorkers, SemCtl(sem->id(), 0, GETVAL, 0));
        }
        // Let all owners exit and replay without removing any set.
        for (int i = 0; i < kWorkers; ++i) {
            ASSERT_TRUE(WriteExact(release_write.get(), &token, 1));
        }
        for (auto& child : children) {
            WaitChildOk(child.get());
        }
        for (auto& sem : sets) {
            EXPECT_EQ(0, SemCtl(sem->id(), 0, GETVAL, 0));
        }
    }
}

TEST(SysVSem, ConcurrentWorkers) {
    constexpr int kWorkers = 8;
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETVAL, kWorkers));

    pid_t children[kWorkers];
    for (int i = 0; i < kWorkers; ++i) {
        children[i] = fork();
        ASSERT_GE(children[i], 0);
        if (children[i] == 0) {
            // Each (-1, +1) pair may block on -1 and release on +1; concurrent races
            // exercise atomicity and wakeup behavior.
            struct sembuf dec = {0, -1, 0};
            struct sembuf inc = {0, 1, 0};
            for (int j = 0; j < 100; ++j) {
                if (SemOp(sem.id(), &dec, 1) != 0) {
                    _exit(10);
                }
                if (SemOp(sem.id(), &inc, 1) != 0) {
                    _exit(11);
                }
            }
            _exit(0);
        }
    }

    for (int i = 0; i < kWorkers; ++i) {
        WaitChildOk(children[i]);
    }
    EXPECT_EQ(kWorkers, SemCtl(sem.id(), 0, GETVAL, 0)) << "net zero for each worker";
}

int main(int argc, char** argv) {
    g_program_path = argv[0];
    if (argc == 2 && strcmp(argv[1], "--sem-undo-exec-helper") == 0) {
        return 0;
    }
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
