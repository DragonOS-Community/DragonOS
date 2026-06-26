#include <gtest/gtest.h>

#include <errno.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <string.h>
#include <sys/wait.h>

#include "cap_common.h"

template <typename T>
static T* bad_user_ptr(uintptr_t addr) {
    return reinterpret_cast<T*>(addr);
}

static void expect_capset_eperm_after_drop(uint64_t next_effective, uint64_t next_permitted,
                                           uint64_t next_inheritable) {
    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
    if (child == 0) {
        cap_user_data_t zero[2];
        fill_caps_v3(0, 0, 0, zero);
        int drop_errno = capset_errno(_LINUX_CAPABILITY_VERSION_3, 0, zero);
        if (drop_errno != 0) {
            _exit(2);
        }

        cap_user_data_t next[2];
        fill_caps_v3(next_effective, next_permitted, next_inheritable, next);
        int set_errno = capset_errno(_LINUX_CAPABILITY_VERSION_3, 0, next);
        _exit(set_errno == EPERM ? 0 : 3);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

static void signal_child_and_expect_success(int pipe_fd, pid_t child) {
    const char done = 'x';
    EXPECT_EQ(1, write(pipe_fd, &done, 1));
    close(pipe_fd);

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(CapGet, CurrentPidVersionV1V2V3) {
    cap_user_data_t data_v1[_LINUX_CAPABILITY_U32S_1] = {};
    EXPECT_EQ(0, capget_errno(_LINUX_CAPABILITY_VERSION_1, 0, data_v1));

    cap_user_data_t data_v2[_LINUX_CAPABILITY_U32S_2] = {};
    EXPECT_EQ(0, capget_errno(_LINUX_CAPABILITY_VERSION_2, 0, data_v2));

    cap_user_data_t data_v3[_LINUX_CAPABILITY_U32S_3] = {};
    EXPECT_EQ(0, capget_errno(_LINUX_CAPABILITY_VERSION_3, 0, data_v3));
}

TEST(CapGet, InvalidVersionProbe) {
    cap_user_header_t hdr = {.version = 0xDEADBEEFu, .pid = 0};
    int ret = syscall(SYS_capget, &hdr, nullptr);
    EXPECT_EQ(0, ret) << "errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_EQ(_LINUX_CAPABILITY_VERSION_3, hdr.version);
}

TEST(CapGet, InvalidVersionWritebackPreservesChildCowHeader) {
    void* mapping = mmap(nullptr, getpagesize(), PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping) << "mmap failed: errno=" << errno << " (" << strerror(errno)
                                   << ")";
    auto* hdr = static_cast<cap_user_header_t*>(mapping);
    hdr->version = 0xDEADBEEFu;
    hdr->pid = 0;

    int sync_pipe[2];
    ASSERT_EQ(0, pipe(sync_pipe));

    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
    if (child == 0) {
        close(sync_pipe[1]);
        char done = 0;
        if (read(sync_pipe[0], &done, 1) != 1) {
            _exit(2);
        }
        close(sync_pipe[0]);
        _exit(hdr->version == 0xDEADBEEFu ? 0 : 3);
    }

    close(sync_pipe[0]);
    errno = 0;
    int ret = syscall(SYS_capget, hdr, nullptr);
    int saved_errno = errno;

    EXPECT_EQ(0, ret) << "errno=" << saved_errno << " (" << strerror(saved_errno) << ")";
    EXPECT_EQ(_LINUX_CAPABILITY_VERSION_3, hdr->version);
    signal_child_and_expect_success(sync_pipe[1], child);
    munmap(mapping, getpagesize());
}

TEST(CapGet, InvalidVersionWithData) {
    cap_user_data_t data[_LINUX_CAPABILITY_U32S_3] = {};
    EXPECT_EQ(EINVAL, capget_errno(0xCAFEBABEu, 0, data));
}

TEST(CapGet, NegativePid) {
    cap_user_data_t data[_LINUX_CAPABILITY_U32S_3] = {};
    EXPECT_EQ(EINVAL, capget_errno(_LINUX_CAPABILITY_VERSION_3, -1, data));
}

TEST(CapGet, NullDataptrWithValidVersion) {
    cap_user_header_t hdr = {.version = _LINUX_CAPABILITY_VERSION_3, .pid = 0};
    errno = 0;
    int ret = syscall(SYS_capget, &hdr, nullptr);
    EXPECT_EQ(0, ret) << "errno=" << errno << " (" << strerror(errno) << ")";
}

TEST(CapGet, NullDataptrReturnsBeforePidRead) {
    cap_user_header_t hdr = {.version = _LINUX_CAPABILITY_VERSION_3, .pid = -1};
    errno = 0;
    int ret = syscall(SYS_capget, &hdr, nullptr);
    EXPECT_EQ(0, ret) << "errno=" << errno << " (" << strerror(errno) << ")";
}

TEST(CapGet, InvalidUserPointersReturnEfault) {
    cap_user_data_t data[_LINUX_CAPABILITY_U32S_3] = {};

    errno = 0;
    int ret = syscall(SYS_capget, bad_user_ptr<cap_user_header_t>(0xdeadbeef), data);
    ASSERT_EQ(-1, ret);
    EXPECT_EQ(EFAULT, errno);

    cap_user_header_t hdr = {.version = _LINUX_CAPABILITY_VERSION_3, .pid = 0};
    errno = 0;
    ret = syscall(SYS_capget, &hdr, bad_user_ptr<cap_user_data_t>(0xcafebabe));
    ASSERT_EQ(-1, ret);
    EXPECT_EQ(EFAULT, errno);
}

TEST(CapGet, PidNotExist) {
    cap_user_data_t data[_LINUX_CAPABILITY_U32S_3] = {};
    EXPECT_EQ(ESRCH, capget_errno(_LINUX_CAPABILITY_VERSION_3, 999999, data));
}

TEST(CapGet, NonZeroPidReturnsTargetCred) {
    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        cap_user_data_t zeros[2];
        fill_caps_v3(0, 0, 0, zeros);
        cap_user_header_t hdr = {.version = _LINUX_CAPABILITY_VERSION_3, .pid = 0};
        if (syscall(SYS_capset, &hdr, zeros) != 0) {
            _exit(1);
        }
        sleep(2);
        _exit(0);
    }

    sleep(1);
    cap_user_header_t hdr = {.version = _LINUX_CAPABILITY_VERSION_3, .pid = child};
    cap_user_data_t data[2] = {};
    ASSERT_EQ(0, syscall(SYS_capget, &hdr, data))
        << "capget(pid=" << child << ") failed: errno=" << errno << " (" << strerror(errno)
        << ")";

    EXPECT_EQ(0u, data[0].effective);
    EXPECT_EQ(0u, data[0].permitted);
    EXPECT_EQ(0u, data[0].inheritable);
    EXPECT_EQ(0u, data[1].effective);
    EXPECT_EQ(0u, data[1].permitted);
    EXPECT_EQ(0u, data[1].inheritable);

    int status = 0;
    EXPECT_EQ(child, waitpid(child, &status, 0));
}

TEST(CapGet, NonZeroPidBasicSuccess) {
    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        _exit(0);
    }

    cap_user_header_t hdr = {.version = _LINUX_CAPABILITY_VERSION_3, .pid = child};
    cap_user_data_t data[2] = {};
    EXPECT_EQ(0, syscall(SYS_capget, &hdr, data))
        << "capget(pid=" << child << ") failed: errno=" << errno << " (" << strerror(errno)
        << ")";

    int status = 0;
    EXPECT_EQ(child, waitpid(child, &status, 0));
}

TEST(CapGet, DataWritePreservesChildCowPage) {
    void* mapping = mmap(nullptr, getpagesize(), PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping) << "mmap failed: errno=" << errno << " (" << strerror(errno)
                                   << ")";
    auto* data = static_cast<cap_user_data_t*>(mapping);
    const cap_user_data_t sentinel[2] = {
        {.effective = 0xA5A5A5A5u, .permitted = 0x5A5A5A5Au, .inheritable = 0x13579BDFu},
        {.effective = 0x2468ACE0u, .permitted = 0x11223344u, .inheritable = 0x55667788u},
    };
    memcpy(data, sentinel, sizeof(sentinel));

    int sync_pipe[2];
    ASSERT_EQ(0, pipe(sync_pipe));

    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
    if (child == 0) {
        close(sync_pipe[1]);
        char done = 0;
        if (read(sync_pipe[0], &done, 1) != 1) {
            _exit(2);
        }
        close(sync_pipe[0]);
        _exit(memcmp(data, sentinel, sizeof(sentinel)) == 0 ? 0 : 3);
    }

    close(sync_pipe[0]);
    cap_user_header_t hdr = {.version = _LINUX_CAPABILITY_VERSION_3, .pid = 0};
    errno = 0;
    int ret = syscall(SYS_capget, &hdr, data);
    int saved_errno = errno;

    EXPECT_EQ(0, ret) << "errno=" << saved_errno << " (" << strerror(saved_errno) << ")";
    EXPECT_NE(0, memcmp(data, sentinel, sizeof(sentinel)));
    signal_child_and_expect_success(sync_pipe[1], child);
    munmap(mapping, getpagesize());
}

TEST(CapSet, EffectiveMustBeSubsetOfPermitted) {
    cap_user_data_t data[2];
    fill_caps_v3(0x1ull, 0x0ull, 0x0ull, data);
    EXPECT_EQ(EPERM, capset_errno(_LINUX_CAPABILITY_VERSION_3, 0, data));
}

TEST(CapSet, VersionPaths) {
    cap_user_data_t data_v1[_LINUX_CAPABILITY_U32S_1] = {};
    EXPECT_EQ(0, capset_errno(_LINUX_CAPABILITY_VERSION_1, 0, data_v1));

    cap_user_data_t data_v2[_LINUX_CAPABILITY_U32S_2] = {};
    EXPECT_EQ(0, capset_errno(_LINUX_CAPABILITY_VERSION_2, 0, data_v2));

    cap_user_data_t data_v3[_LINUX_CAPABILITY_U32S_3] = {};
    EXPECT_EQ(0, capset_errno(_LINUX_CAPABILITY_VERSION_3, 0, data_v3));
}

TEST(CapSet, InvalidVersionWithData) {
    cap_user_data_t data[_LINUX_CAPABILITY_U32S_3] = {};
    EXPECT_EQ(EINVAL, capset_errno(0xCAFEBABEu, 0, data));
}

TEST(CapSet, InvalidVersionWritesBackKernelVersion) {
    cap_user_header_t hdr = {.version = 0xCAFEBABEu, .pid = 0};
    errno = 0;
    int ret = syscall(SYS_capset, &hdr, bad_user_ptr<cap_user_data_t>(0xcafebabe));
    ASSERT_EQ(-1, ret);
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(_LINUX_CAPABILITY_VERSION_3, hdr.version);
}

TEST(CapSet, InvalidVersionWritebackPreservesChildCowHeader) {
    void* mapping = mmap(nullptr, getpagesize(), PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping) << "mmap failed: errno=" << errno << " (" << strerror(errno)
                                   << ")";
    auto* hdr = static_cast<cap_user_header_t*>(mapping);
    hdr->version = 0xCAFEBABEu;
    hdr->pid = 0;

    int sync_pipe[2];
    ASSERT_EQ(0, pipe(sync_pipe));

    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
    if (child == 0) {
        close(sync_pipe[1]);
        char done = 0;
        if (read(sync_pipe[0], &done, 1) != 1) {
            _exit(2);
        }
        close(sync_pipe[0]);
        _exit(hdr->version == 0xCAFEBABEu ? 0 : 3);
    }

    close(sync_pipe[0]);
    errno = 0;
    int ret = syscall(SYS_capset, hdr, bad_user_ptr<cap_user_data_t>(0xcafebabe));
    int saved_errno = errno;

    EXPECT_EQ(-1, ret);
    EXPECT_EQ(EINVAL, saved_errno);
    EXPECT_EQ(_LINUX_CAPABILITY_VERSION_3, hdr->version);
    signal_child_and_expect_success(sync_pipe[1], child);
    munmap(mapping, getpagesize());
}

TEST(CapSet, InvalidUserPointersReturnEfault) {
    errno = 0;
    int ret = syscall(SYS_capset, nullptr, nullptr);
    ASSERT_EQ(-1, ret);
    EXPECT_EQ(EFAULT, errno);

    errno = 0;
    ret = syscall(SYS_capset, bad_user_ptr<cap_user_header_t>(0xdeadbeef),
                  bad_user_ptr<cap_user_data_t>(0xcafebabe));
    ASSERT_EQ(-1, ret);
    EXPECT_EQ(EFAULT, errno);

    cap_user_header_t hdr = {.version = _LINUX_CAPABILITY_VERSION_3, .pid = 0};
    errno = 0;
    ret = syscall(SYS_capset, &hdr, bad_user_ptr<cap_user_data_t>(0xcafebabe));
    ASSERT_EQ(-1, ret);
    EXPECT_EQ(EFAULT, errno);
}

TEST(CapSet, NegativePid) {
    cap_user_data_t data[_LINUX_CAPABILITY_U32S_3] = {};
    EXPECT_EQ(EPERM, capset_errno(_LINUX_CAPABILITY_VERSION_3, -1, data));
}

TEST(CapSet, NonCurrentPid) {
    cap_user_data_t data[_LINUX_CAPABILITY_U32S_3] = {};
    EXPECT_EQ(EPERM, capset_errno(_LINUX_CAPABILITY_VERSION_3, 999999, data));
}

TEST(CapSet, NonCurrentPidReturnsEpermBeforeDataRead) {
    cap_user_header_t hdr = {.version = _LINUX_CAPABILITY_VERSION_3, .pid = 999999};
    errno = 0;
    int ret = syscall(SYS_capset, &hdr, bad_user_ptr<cap_user_data_t>(0xcafebabe));
    ASSERT_EQ(-1, ret);
    EXPECT_EQ(EPERM, errno);
}

TEST(CapSet, PermittedNotIncrease) {
    // 子进程先降权到 pP=0，再尝试提升 pP(bit0)，应触发 EPERM
    expect_capset_eperm_after_drop(0, 1, 0);
}

TEST(CapSet, InheritableBounds) {
    // 子进程先降权到 pI=0,pP=0，再尝试提升 pI(bit0)，应触发 EPERM
    expect_capset_eperm_after_drop(0, 0, 1);
}

static uint64_t current_permitted_caps() {
    cap_user_data_t data[2] = {};
    int err = capget_errno(_LINUX_CAPABILITY_VERSION_3, 0, data);
    if (err != 0) {
        return 0;
    }
    return cap_permitted_u64(data);
}

TEST(PrctlKeepCaps, GetOptionNumberLinuxCompatible) {
    errno = 0;
    long ret = syscall(SYS_prctl, 7, 0, 0, 0, 0);
    ASSERT_NE(-1, ret) << "prctl(PR_GET_KEEPCAPS=7) failed: errno=" << errno << " ("
                       << strerror(errno) << ")";
    EXPECT_TRUE(ret == 0 || ret == 1) << "unexpected PR_GET_KEEPCAPS value: " << ret;
}

TEST(PrctlKeepCaps, SetRejectsInvalidValue) {
    errno = 0;
    long ret = syscall(SYS_prctl, PR_SET_KEEPCAPS, 2, 0, 0, 0);
    ASSERT_EQ(-1, ret);
    EXPECT_EQ(EINVAL, errno);
}

TEST(PrctlKeepCaps, SetuidDropWithoutKeepCapsClearsPermitted) {
    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
    if (child == 0) {
        // 仅在 root 场景验证；非 root 环境下跳过（退出码 0）。
        if (geteuid() != 0) {
            _exit(0);
        }

        if (syscall(SYS_prctl, PR_SET_KEEPCAPS, 0, 0, 0, 0) != 0) {
            _exit(2);
        }

        if (setuid(1000) != 0) {
            _exit(3);
        }

        uint64_t p = current_permitted_caps();
        _exit(p == 0 ? 0 : 4);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(PrctlKeepCaps, SetuidDropWithKeepCapsRetainsPermitted) {
    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
    if (child == 0) {
        // 仅在 root 场景验证；非 root 环境下跳过（退出码 0）。
        if (geteuid() != 0) {
            _exit(0);
        }

        if (syscall(SYS_prctl, PR_SET_KEEPCAPS, 1, 0, 0, 0) != 0) {
            _exit(2);
        }

        cap_user_data_t before[2] = {};
        int before_err = capget_errno(_LINUX_CAPABILITY_VERSION_3, 0, before);
        if (before_err != 0) {
            _exit(6);
        }

        if (setuid(1000) != 0) {
            _exit(3);
        }

        cap_user_data_t data[2] = {};
        int err = capget_errno(_LINUX_CAPABILITY_VERSION_3, 0, data);
        if (err != 0) {
            _exit(4);
        }

        uint64_t p_before = cap_permitted_u64(before);
        uint64_t p = cap_permitted_u64(data);
        uint64_t e = cap_effective_u64(data);
        // Linux 语义：keepcaps 保留 permitted；euid 0->non0 会清除 effective。
        _exit((p == p_before && e == 0) ? 0 : 5);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
