#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <errno.h>
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

void WaitChildOk(pid_t child) {
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0))
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

    struct semid_ds ds;
    int ret = SemCtl(sem.id(), 0, SEM_STAT, reinterpret_cast<unsigned long>(&ds));
    ASSERT_GE(ret, 0) << "SEM_STAT failed: errno=" << errno;
    EXPECT_EQ(1u, ds.sem_nsems);

    ret = SemCtl(sem.id(), 0, SEM_STAT_ANY, reinterpret_cast<unsigned long>(&ds));
    ASSERT_GE(ret, 0) << "SEM_STAT_ANY failed: errno=" << errno;
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

TEST(SysVSem, IpcInfoAndSemInfo) {
    SemSet sem(1, IPC_CREAT | 0600);
    ASSERT_TRUE(sem.valid());

    struct seminfo info;
    memset(&info, 0, sizeof(info));
    int max_id = SemCtl(sem.id(), 0, IPC_INFO, reinterpret_cast<unsigned long>(&info));
    EXPECT_GE(max_id, 0);
    EXPECT_GT(info.semvmx, 0);
    EXPECT_EQ(500, info.semopm);

    memset(&info, 0, sizeof(info));
    max_id = SemCtl(sem.id(), 0, SEM_INFO, reinterpret_cast<unsigned long>(&info));
    EXPECT_GE(max_id, 0);
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

    const pid_t child = fork();
    ASSERT_GE(child, 0);

    if (child == 0) {
        struct sigaction sa = {};
        sa.sa_handler = [](int) {};
        sigemptyset(&sa.sa_mask);
        ASSERT_EQ(0, sigaction(SIGUSR1, &sa, nullptr)) << "sigaction failed";

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
    ASSERT_EQ(0, kill(child, SIGUSR1)) << "kill failed: errno=" << errno;
    WaitChildOk(child);
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
