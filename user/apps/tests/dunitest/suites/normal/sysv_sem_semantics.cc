#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <errno.h>
#include <limits.h>
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
#include <unistd.h>

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

#ifndef SEM_STAT
#define SEM_STAT 18
#endif
#ifndef SEM_STAT_ANY
#define SEM_STAT_ANY 20
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

    // 接管已存在的 id
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

// 等待指定信号量的等待者数量达到预期（用于确认子进程已阻塞）
bool WaitForWaiters(int semid, int semnum, int expected, int timeout_ms = 5000) {
    const int deadline = timeout_ms * 1000;
    for (int elapsed = 0; elapsed < deadline; elapsed += 10000) {
        int ncnt = SemCtl(semid, semnum, GETNCNT, 0);
        int zcnt = SemCtl(semid, semnum, GETZCNT, 0);
        if (ncnt >= 0 && ncnt + zcnt >= expected) {
            return true;
        }
        usleep(10000);
    }
    return false;
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

TEST(SysVSem, SetAllAtomicRangeError) {
    SemSet sem(3, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    unsigned short ok[3] = {1, 2, 3};
    ASSERT_EQ(0, SemCtl(sem.id(), 0, SETALL, reinterpret_cast<unsigned long>(ok)));

    unsigned short bad[3] = {1, 40000, 3};
    errno = 0;
    EXPECT_EQ(-1, SemCtl(sem.id(), 0, SETALL, reinterpret_cast<unsigned long>(bad)));
    EXPECT_EQ(ERANGE, errno);

    // 校验失败不得改变任何值
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

    // semval + op > SEMVMX：立即 ERANGE，不阻塞、不改变任何值
    struct sembuf inc = {0, 2, 0};
    errno = 0;
    EXPECT_EQ(-1, SemOp(sem.id(), &inc, 1));
    EXPECT_EQ(ERANGE, errno);
    EXPECT_EQ(32766, SemCtl(sem.id(), 0, GETVAL, 0)) << "no change on ERANGE";
}

TEST(SysVSem, WaitForZero) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    // val == 0：立即成功
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

    // op1 成功（1-1=0），op2 失败（0-1<0，IPC_NOWAIT）→ 整体 EAGAIN，op1 回滚
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

TEST(SysVSem, SemUndoRejected) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    struct sembuf op = {0, 1, SEM_UNDO};
    errno = 0;
    EXPECT_EQ(-1, SemOp(sem.id(), &op, 1));
    EXPECT_EQ(ENOSYS, errno) << "SEM_UNDO unsupported";
}

// ============ blocking & wakeup ============

TEST(SysVSem, BlockingWakeup) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    const pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno;

    if (child == 0) {
        // 子进程阻塞在 -1（val==0）
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
        // 子进程等待 val 变 0
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

    // 一次 +kWaiters 唤醒全部等待者
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
        // 阻塞被打断：必须返回 EINTR
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
            // 每对 (-1, +1)：-1 可能阻塞，+1 释放，并发竞争测原子性与唤醒
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
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
