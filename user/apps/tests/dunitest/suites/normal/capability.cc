#include <gtest/gtest.h>

#include <errno.h>
#include <string.h>
#include <sys/wait.h>

#include "cap_common.h"

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
    int saved_errno = errno;
    bool ok = (ret == 0) || (ret == -1 && saved_errno == EINVAL);
    EXPECT_TRUE(ok) << "ret=" << ret << ", errno=" << saved_errno << " (" << strerror(saved_errno)
                    << ")";
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

TEST(CapSet, NegativePid) {
    cap_user_data_t data[_LINUX_CAPABILITY_U32S_3] = {};
    EXPECT_EQ(EPERM, capset_errno(_LINUX_CAPABILITY_VERSION_3, -1, data));
}

TEST(CapSet, NonCurrentPid) {
    cap_user_data_t data[_LINUX_CAPABILITY_U32S_3] = {};
    EXPECT_EQ(EPERM, capset_errno(_LINUX_CAPABILITY_VERSION_3, 999999, data));
}

TEST(CapSet, PermittedNotIncrease) {
    // 子进程先降权到 pP=0，再尝试提升 pP(bit0)，应触发 EPERM
    expect_capset_eperm_after_drop(0, 1, 0);
}

TEST(CapSet, InheritableBounds) {
    // 子进程先降权到 pI=0,pP=0，再尝试提升 pI(bit0)，应触发 EPERM
    expect_capset_eperm_after_drop(0, 0, 1);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
