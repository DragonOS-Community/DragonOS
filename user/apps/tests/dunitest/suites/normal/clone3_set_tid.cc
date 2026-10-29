#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <linux/sched.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

long CloneWithTids(const pid_t* tids, size_t count, uint64_t flags = 0,
                   uint64_t pidfd = 0, pid_t* parent_tid = nullptr,
                   pid_t* child_tid = nullptr) {
    struct clone_args args = {};
    args.flags = flags;
    args.exit_signal = SIGCHLD;
    args.set_tid = reinterpret_cast<uintptr_t>(tids);
    args.set_tid_size = count;
    args.pidfd = pidfd;
    args.parent_tid = reinterpret_cast<uintptr_t>(parent_tid);
    args.child_tid = reinterpret_cast<uintptr_t>(child_tid);
    return syscall(SYS_clone3, &args, sizeof(args));
}

void Reap(pid_t child) {
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

void ExpectCloneError(const pid_t* tids, size_t count, int expected,
                      uint64_t flags = 0, uint64_t pidfd = 0) {
    errno = 0;
    long child = CloneWithTids(tids, count, flags, pidfd);
    int error = errno;
    if (child == 0) _exit(90);
    if (child > 0) {
        kill(child, SIGKILL);
        waitpid(child, nullptr, 0);
    }
    EXPECT_EQ(child, -1);
    EXPECT_EQ(error, expected);
}

bool SetOnlyCapability(unsigned capability, bool enabled = true) {
    struct __user_cap_header_struct header = {};
    struct __user_cap_data_struct data[2] = {};
    header.version = _LINUX_CAPABILITY_VERSION_3;
    if (enabled) {
        data[capability / 32].effective = 1U << (capability % 32);
        data[capability / 32].permitted = data[capability / 32].effective;
    }
    return syscall(SYS_capset, &header, data) == 0;
}

int ReadPidMax() {
    int fd = open("/proc/sys/kernel/pid_max", O_RDONLY | O_CLOEXEC);
    if (fd < 0) return -1;
    char value[32] = {};
    ssize_t length = read(fd, value, sizeof(value) - 1);
    close(fd);
    int maximum = -1;
    if (length <= 0 || sscanf(value, "%d", &maximum) != 1) return -1;
    return maximum;
}

bool WritePidMax(const char* value) {
    int fd = open("/proc/sys/kernel/pid_max", O_WRONLY | O_CLOEXEC);
    if (fd < 0) return false;
    ssize_t result = write(fd, value, strlen(value));
    int error = errno;
    close(fd);
    errno = error;
    return result == static_cast<ssize_t>(strlen(value));
}

class PidMaxRestore {
public:
    PidMaxRestore() : original_(ReadPidMax()) {}
    ~PidMaxRestore() {
        if (original_ > 0) {
            char value[32];
            snprintf(value, sizeof(value), "%d\n", original_);
            EXPECT_TRUE(WritePidMax(value)) << "restoring global pid_max: " << strerror(errno);
        }
    }
    int original() const { return original_; }

private:
    int original_;
};

// Never mutate the test runner's namespace or capabilities. A parent-side
// deadline and process-group cleanup bound even a broken kernel wait path.
void InPidNamespace(void (*body)(), bool start_init = true) {
    fflush(nullptr);
    pid_t supervisor = fork();
    ASSERT_GE(supervisor, 0) << strerror(errno);
    if (supervisor == 0) {
        if (setsid() < 0 || unshare(CLONE_NEWUSER | CLONE_NEWPID) < 0) {
            fprintf(stderr, "clone3 test namespace setup: %s\n", strerror(errno));
            _exit(100);
        }
        alarm(20);
        if (start_init) {
            pid_t init = fork();
            if (init < 0) _exit(101);
            if (init == 0) {
                alarm(15);
                body();
                fflush(nullptr);
                _exit(::testing::Test::HasFailure() ? 1 : 0);
            }
            int status = 0;
            if (waitpid(init, &status, 0) != init || !WIFEXITED(status)) _exit(102);
            _exit(WEXITSTATUS(status));
        }
        body();
        fflush(nullptr);
        _exit(::testing::Test::HasFailure() ? 1 : 0);
    }
    int status = 0;
    pid_t result = 0;
    for (int attempt = 0; attempt < 300; ++attempt) {
        result = waitpid(supervisor, &status, WNOHANG);
        if (result != 0) break;
        usleep(100000);
    }
    // Also remove leaked descendants on an assertion failure.
    kill(-supervisor, SIGKILL);
    if (result == 0) {
        kill(supervisor, SIGKILL);
        waitpid(supervisor, &status, 0);
        FAIL() << "namespace scenario exceeded 30 seconds";
    }
    ASSERT_EQ(result, supervisor) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

void RequestedPid() {
    ASSERT_EQ(getpid(), 1);
    const pid_t requested = 4242;
    long child = CloneWithTids(&requested, 1);
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) _exit(getpid() == requested ? 0 : 1);
    EXPECT_EQ(child, requested);
    Reap(child);
}

}  // namespace

TEST(Clone3SetTid, RequestedPidIsVisibleToParentAndChild) {
    InPidNamespace(RequestedPid);
}

TEST(Clone3SetTid, InvalidNumbersAndTooManyNamespaceLevels) {
    InPidNamespace([] {
        for (pid_t invalid : {0, -1, INT32_MAX}) {
            ExpectCloneError(&invalid, 1, EINVAL);
        }
        const pid_t too_many[] = {4242, 4243, 4244};
        ExpectCloneError(too_many, 3, EINVAL);
        ExpectCloneError(too_many, 33, EINVAL);
    });
}

TEST(Clone3SetTid, NewNamespaceRequiresInitPidOne) {
    InPidNamespace([] {
        const pid_t invalid_init = 4242;
        ExpectCloneError(&invalid_init, 1, EINVAL);
        const pid_t init = 1;
        long child = CloneWithTids(&init, 1);
        ASSERT_GE(child, 0) << strerror(errno);
        if (child == 0) _exit(getpid() == 1 ? 0 : 1);
        Reap(child);
    }, false);
}

TEST(Clone3SetTid, PointerAndSizeMustAgree) {
    InPidNamespace([] {
        const pid_t requested = 4242;
        ExpectCloneError(nullptr, 1, EINVAL);
        ExpectCloneError(&requested, 0, EINVAL);
        ExpectCloneError(reinterpret_cast<const pid_t*>(1), 1, EFAULT);
    });
}

TEST(Clone3SetTid, PublishedPidConflictDoesNotCreateChild) {
    InPidNamespace([] {
        int hold[2];
        ASSERT_EQ(pipe(hold), 0);
        const pid_t requested = 4242;
        long child = CloneWithTids(&requested, 1);
        ASSERT_GE(child, 0) << strerror(errno);
        if (child == 0) {
            close(hold[1]);
            char byte;
            _exit(read(hold[0], &byte, 1) == 0 ? 0 : 1);
        }
        close(hold[0]);
        ExpectCloneError(&requested, 1, EEXIST);
        close(hold[1]);
        Reap(child);
        child = CloneWithTids(&requested, 1);
        ASSERT_GE(child, 0) << strerror(errno);
        if (child == 0) _exit(getpid() == requested ? 0 : 1);
        EXPECT_EQ(child, requested);
        Reap(child);
    });
}

TEST(Clone3SetTid, InnermostFirstArrayAssignsBothNamespaceViews) {
    InPidNamespace([] {
        const pid_t tids[] = {1, 4343};
        long child = CloneWithTids(tids, 2, CLONE_NEWPID);
        ASSERT_GE(child, 0) << strerror(errno);
        if (child == 0) {
            if (getpid() != 1 || getppid() != 0) _exit(1);
            RequestedPid();
            _exit(::testing::Test::HasFailure() ? 1 : 0);
        }
        EXPECT_EQ(child, 4343);
        Reap(child);
    });
}

TEST(Clone3SetTid, ParentAndChildTidWritesUseTheirNamespaceViews) {
    InPidNamespace([] {
        const pid_t tids[] = {1, 4343};
        pid_t parent_tid = -1, child_tid = -1;
        long child = CloneWithTids(tids, 2,
                                  CLONE_NEWPID | CLONE_PARENT_SETTID | CLONE_CHILD_SETTID,
                                  0, &parent_tid, &child_tid);
        ASSERT_GE(child, 0) << strerror(errno);
        if (child == 0) _exit(getpid() == 1 && child_tid == 1 ? 0 : 1);
        EXPECT_EQ(child, 4343);
        EXPECT_EQ(parent_tid, 4343);
        Reap(child);
    });
}

TEST(Clone3SetTid, OrdinaryForkStillUsesAutomaticPidAllocation) {
    InPidNamespace([] {
        RequestedPid();
        pid_t child = fork();
        ASSERT_GE(child, 0) << strerror(errno);
        if (child == 0) _exit(getpid() > 1 ? 0 : 1);
        EXPECT_GT(child, 1);
        // Reserving an exact high ID must not advance the automatic cursor.
        EXPECT_LT(child, 4242);
        Reap(child);
    });
}

TEST(Clone3SetTid, FreshForChildrenNamespaceSurvivesOuterAllocationFailure) {
    InPidNamespace([] {
        // Reuse this very same ns_for_children after the outer-layer failure;
        // retrying CLONE_NEWPID alone would construct a different namespace.
        ASSERT_EQ(unshare(CLONE_NEWPID), 0) << strerror(errno);
        const pid_t collision[] = {1, 1};
        ExpectCloneError(collision, 2, EEXIST);
        const pid_t invalid_outer[] = {1, 0};
        ExpectCloneError(invalid_outer, 2, EINVAL);
        const pid_t valid[] = {1, 4343};
        long child = CloneWithTids(valid, 2);
        ASSERT_GE(child, 0) << strerror(errno);
        if (child == 0) _exit(getpid() == 1 ? 0 : 1);
        EXPECT_EQ(child, 4343);
        Reap(child);
    });
}

TEST(Clone3SetTid, GlobalPidMaxPolicyAndExistingPidLifetime) {
    struct utsname identity = {};
    ASSERT_EQ(uname(&identity), 0);
    if (!strstr(identity.release, "-dragonos") &&
        !strstr(identity.version, "-dragonos") && strcmp(identity.sysname, "DragonOS") != 0) {
        GTEST_SKIP() << "DragonOS-only global pid_max test; never change host policy";
    }

    // This guard lives in the original runner, never a killable namespace
    // scenario. All assertion exits and namespace deadlines restore policy.
    PidMaxRestore restore;
    ASSERT_GT(restore.original(), 0);
    ASSERT_TRUE(WritePidMax("65536\n")) << strerror(errno);
    EXPECT_EQ(ReadPidMax(), 65536);
    for (const char* invalid : {"0\n", "300\n", "4194305\n", "-1\n", "bad\n"}) {
        errno = 0;
        EXPECT_FALSE(WritePidMax(invalid));
        EXPECT_EQ(errno, EINVAL);
        EXPECT_EQ(ReadPidMax(), 65536);
    }
    int hold[2];
    ASSERT_EQ(pipe(hold), 0);
    const pid_t high = 60000;
    long child = CloneWithTids(&high, 1);
    if (child == 0) {
        close(hold[1]);
        alarm(15);
        char byte;
        _exit(getpid() == high && read(hold[0], &byte, 1) == 0 ? 0 : 1);
    }
    close(hold[0]);
    EXPECT_EQ(child, high) << strerror(errno);
    if (child < 0) {
        close(hold[1]);
        return;
    }
    EXPECT_TRUE(WritePidMax("32768\n")) << strerror(errno);
    EXPECT_EQ(ReadPidMax(), 32768);
    // Reducing the allocation boundary must not invalidate a live old ID.
    EXPECT_EQ(kill(child, 0), 0) << strerror(errno);
    close(hold[1]);
    int status = 0;
    pid_t waited = 0;
    for (int attempt = 0; attempt < 200; ++attempt) {
        waited = waitpid(child, &status, WNOHANG);
        if (waited != 0) break;
        usleep(100000);
    }
    if (waited == 0) {
        kill(child, SIGKILL);
        waited = waitpid(child, &status, 0);
        ADD_FAILURE() << "old high PID did not exit within 20 seconds";
    }
    EXPECT_EQ(waited, child);
    EXPECT_TRUE(WIFEXITED(status));
    if (WIFEXITED(status)) {
        EXPECT_EQ(WEXITSTATUS(status), 0);
    }
    InPidNamespace([] {
        const pid_t boundary = 32768;
        ExpectCloneError(&boundary, 1, EINVAL);
        const pid_t allowed = 32767;
        long child = CloneWithTids(&allowed, 1);
        ASSERT_GE(child, 0) << strerror(errno);
        if (child == 0) _exit(getpid() == allowed ? 0 : 1);
        EXPECT_EQ(child, allowed);
        Reap(child);
        child = fork();
        ASSERT_GE(child, 0) << strerror(errno);
        if (child == 0) _exit(getpid() > 1 && getpid() < 32768 ? 0 : 1);
        EXPECT_GT(child, 1);
        EXPECT_LT(child, 32768);
        Reap(child);
    });
    // Global sysctl DAC checks global euid, not capabilities in a userns.
    // Unsharing as global uid 0 does not make a non-owner caller.
    pid_t nonowner = fork();
    ASSERT_GE(nonowner, 0);
    if (nonowner == 0) {
        alarm(15);
        if (setresuid(65534, 65534, 65534) != 0) _exit(1);
        errno = 0;
        const bool accepted = WritePidMax("65536\n");
        _exit(!accepted && errno == EACCES ? 0 : 1);
    }
    Reap(nonowner);
}

TEST(Clone3SetTid, SysAdminCapabilityIsSufficient) {
    InPidNamespace([] {
        ASSERT_TRUE(SetOnlyCapability(CAP_SYS_ADMIN)) << strerror(errno);
        RequestedPid();
    });
}

TEST(Clone3SetTid, CheckpointRestoreCapabilityIsSufficient) {
    InPidNamespace([] {
        // Linux 6.6 CAP_CHECKPOINT_RESTORE; retain compatibility with older
        // userspace headers without changing the tested capability number.
        ASSERT_TRUE(SetOnlyCapability(40)) << strerror(errno);
        RequestedPid();
    });
}

TEST(Clone3SetTid, RequestedPidNeedsNamespaceOwnerCapability) {
    InPidNamespace([] {
        ASSERT_TRUE(SetOnlyCapability(0, false)) << strerror(errno);
        const pid_t requested = 4242;
        ExpectCloneError(&requested, 1, EPERM);
        // Automatic PID allocation is not privileged.
        pid_t child = fork();
        ASSERT_GE(child, 0) << strerror(errno);
        if (child == 0) _exit(0);
        Reap(child);
    });
}

TEST(Clone3SetTid, OuterPermissionFailureRollsBackInnerReservation) {
    InPidNamespace([] {
        // The test owns the new user namespace, not its initial-user-ns
        // ancestor. Permission must be checked separately at every level.
        const pid_t tids[] = {4242, 4343};
        ExpectCloneError(tids, 2, EPERM);
        RequestedPid();
    });
}

TEST(Clone3SetTid, OuterCollisionDoesNotPublishNewPidNamespace) {
    InPidNamespace([] {
        const pid_t collision[] = {1, 1};
        ExpectCloneError(collision, 2, EEXIST, CLONE_NEWPID);
        const pid_t valid[] = {1, 4343};
        long child = CloneWithTids(valid, 2, CLONE_NEWPID);
        ASSERT_GE(child, 0) << strerror(errno);
        if (child == 0) _exit(getpid() == 1 ? 0 : 1);
        EXPECT_EQ(child, 4343);
        Reap(child);
    });
}

TEST(Clone3SetTid, PidfdUserWriteFailureAllowsRequestedPidReuse) {
    InPidNamespace([] {
        const pid_t requested = 4242;
        ExpectCloneError(&requested, 1, EFAULT, CLONE_PIDFD, 1);
        RequestedPid();
    });
}

TEST(Clone3SetTid, CrossPageTidArrayFaultIsProtected) {
    InPidNamespace([] {
        const long page = sysconf(_SC_PAGESIZE);
        ASSERT_GT(page, 0);
        void* map = mmap(nullptr, 2 * page, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        ASSERT_NE(map, MAP_FAILED);
        auto* tids = reinterpret_cast<pid_t*>(static_cast<char*>(map) + page - sizeof(pid_t));
        tids[0] = 4242;
        ASSERT_EQ(mprotect(static_cast<char*>(map) + page, page, PROT_NONE), 0);
        ExpectCloneError(tids, 2, EFAULT);
        EXPECT_EQ(munmap(map, 2 * page), 0);
        RequestedPid();
    });
}

TEST(Clone3SetTid, ConcurrentRequestsPublishExactlyOnePid) {
    InPidNamespace([] {
        int results[2], hold[2];
        ASSERT_EQ(pipe(results), 0);
        ASSERT_EQ(pipe(hold), 0);
        constexpr int workers = 8;
        pid_t pids[workers];
        for (int i = 0; i < workers; ++i) {
            pids[i] = fork();
            ASSERT_GE(pids[i], 0) << strerror(errno);
            if (pids[i] == 0) {
                close(results[0]);
                close(hold[1]);
                const pid_t requested = 4242;
                long child = CloneWithTids(&requested, 1);
                int error = errno;
                if (child == 0) {
                    close(results[1]);
                    char byte;
                    _exit(getpid() == requested && read(hold[0], &byte, 1) == 0 ? 0 : 1);
                }
                const int result = child == requested ? 0 : child == -1 ? error : EINVAL;
                if (write(results[1], &result, sizeof(result)) != sizeof(result)) _exit(1);
                close(results[1]);
                close(hold[0]);
                if (child > 0) {
                    int status = 0;
                    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
                        WEXITSTATUS(status) != 0) _exit(1);
                }
                _exit(0);
            }
        }
        close(results[1]);
        close(hold[0]);
        int winners = 0;
        for (int i = 0; i < workers; ++i) {
            int result;
            ASSERT_EQ(read(results[0], &result, sizeof(result)), static_cast<ssize_t>(sizeof(result)));
            if (result == 0) ++winners;
            else EXPECT_EQ(result, EEXIST);
        }
        EXPECT_EQ(winners, 1);
        close(results[0]);
        close(hold[1]);
        for (pid_t child : pids) Reap(child);
        RequestedPid();
    });
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
