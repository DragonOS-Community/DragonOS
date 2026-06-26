#include <stdio.h>
#include <stdint.h>
#include <errno.h>
#include <string.h>
#include <unistd.h>
#include <sys/syscall.h>
#include <sys/wait.h>

typedef struct {
    uint32_t version;
    int32_t pid;
} cap_user_header_t;

typedef struct {
    uint32_t effective;
    uint32_t permitted;
    uint32_t inheritable;
} cap_user_data_t;

// Linux 版本常量
#define _LINUX_CAPABILITY_VERSION_1 0x19980330
#define _LINUX_CAPABILITY_VERSION_2 0x20071026
#define _LINUX_CAPABILITY_VERSION_3 0x20080522

#define _LINUX_CAPABILITY_U32S_1 1
#define _LINUX_CAPABILITY_U32S_2 2
#define _LINUX_CAPABILITY_U32S_3 2

static int do_capset(uint32_t version, int32_t pid,
                     cap_user_data_t *data, size_t elems,
                     int expect_errno) {
    cap_user_header_t hdr = { .version = version, .pid = pid };
    int ret = syscall(SYS_capset, &hdr, data);
    if (ret < 0) {
        int e = errno;
        if (expect_errno == 0) {
            printf("[FAIL] capset(version=0x%x,pid=%d) ret=%d errno=%d(%s), expected success\n",
                   version, pid, ret, e, strerror(e));
            return -1;
        }
        if (e != expect_errno) {
            printf("[FAIL] capset(version=0x%x,pid=%d) errno=%d(%s), expected %d(%s)\n",
                   version, pid, e, strerror(e), expect_errno, strerror(expect_errno));
            return -1;
        }
        printf("[PASS] capset(version=0x%x,pid=%d) failed as expected with errno=%d(%s)\n",
               version, pid, e, strerror(e));
        return 0;
    } else {
        if (expect_errno != 0) {
            printf("[FAIL] capset(version=0x%x,pid=%d) succeeded, expected errno=%d\n",
                   version, pid, expect_errno);
            return -1;
        }
        printf("[PASS] capset(version=0x%x,pid=%d) succeeded\n", version, pid);
        return 0;
    }
}

// 构造 v3 两元素数组（低32/高32）
static void fill_caps_v3(uint64_t e, uint64_t p, uint64_t i,
                         cap_user_data_t out[2]) {
    out[0].effective   = (uint32_t)(e & 0xFFFFFFFFu);
    out[0].permitted   = (uint32_t)(p & 0xFFFFFFFFu);
    out[0].inheritable = (uint32_t)(i & 0xFFFFFFFFu);

    out[1].effective   = (uint32_t)((e >> 32) & 0xFFFFFFFFu);
    out[1].permitted   = (uint32_t)((p >> 32) & 0xFFFFFFFFu);
    out[1].inheritable = (uint32_t)((i >> 32) & 0xFFFFFFFFu);
}

static int test_rule_effective_subset_permitted() {
    // 期望：pE ⊆ pP 才允许。构造 pE 有 bit0，pP 无 bit0 → EPERM
    cap_user_data_t data[2];
    fill_caps_v3(0x1ull, 0x0ull, 0x0ull, data);
    return do_capset(_LINUX_CAPABILITY_VERSION_3, 0, data, 2, EPERM) == 0 ? 0 : -1;
}

static int expect_capset_eperm_after_drop(uint64_t next_effective,
                                          uint64_t next_permitted,
                                          uint64_t next_inheritable) {
    pid_t child = fork();
    if (child < 0) {
        printf("[FAIL] fork failed: errno=%d(%s)\n", errno, strerror(errno));
        return -1;
    }

    if (child == 0) {
        cap_user_data_t zero[2];
        fill_caps_v3(0, 0, 0, zero);
        if (do_capset(_LINUX_CAPABILITY_VERSION_3, 0, zero, 2, 0) != 0) {
            _exit(11);
        }

        cap_user_data_t next[2];
        fill_caps_v3(next_effective, next_permitted, next_inheritable, next);
        if (do_capset(_LINUX_CAPABILITY_VERSION_3, 0, next, 2, EPERM) != 0) {
            _exit(12);
        }

        _exit(0);
    }

    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        printf("[FAIL] waitpid(%d) failed: errno=%d(%s)\n",
               child, errno, strerror(errno));
        return -1;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        printf("[FAIL] child %d exited abnormally: status=0x%x\n", child, status);
        return -1;
    }
    return 0;
}

static int test_rule_permitted_not_increase() {
    // 期望：pP_new ⊆ pP_old。子进程先清空 pP，再尝试提升 bit0 → EPERM
    return expect_capset_eperm_after_drop(0, 1, 0);
}

static int test_rule_inheritable_bounds() {
    // 期望：无 CAP_SETPCAP 时 pI_new ⊆ pI_old ∪ pP_old；先清空 pI/pP，再提升 pI(bit0) → EPERM
    return expect_capset_eperm_after_drop(0, 0, 1);
}

static int test_version_paths() {
    // v1：使用 1 元素
    cap_user_data_t data1[_LINUX_CAPABILITY_U32S_1] = {0};
    if (do_capset(_LINUX_CAPABILITY_VERSION_1, 0, data1, _LINUX_CAPABILITY_U32S_1, 0) != 0) return -1;

    // v2：使用 2 元素
    cap_user_data_t data2[_LINUX_CAPABILITY_U32S_2] = {0};
    if (do_capset(_LINUX_CAPABILITY_VERSION_2, 0, data2, _LINUX_CAPABILITY_U32S_2, 0) != 0) return -1;

    // v3：使用 2 元素
    cap_user_data_t data3[_LINUX_CAPABILITY_U32S_3] = {0};
    if (do_capset(_LINUX_CAPABILITY_VERSION_3, 0, data3, _LINUX_CAPABILITY_U32S_3, 0) != 0) return -1;

    // 版本无效 + dataptr 非 NULL → EINVAL
    cap_user_data_t data_bad[_LINUX_CAPABILITY_U32S_3] = {0};
    if (do_capset(0xCAFEBABE, 0, data_bad, _LINUX_CAPABILITY_U32S_3, EINVAL) != 0) return -1;

    // pid 为负数：Linux 观测为 EPERM（DragonOS 可能不同），按 Linux 预期调整
    if (do_capset(_LINUX_CAPABILITY_VERSION_3, -1, data3, _LINUX_CAPABILITY_U32S_3, EPERM) != 0) return -1;

    // 非当前 pid → EPERM（DragonOS 最小实现）
    if (do_capset(_LINUX_CAPABILITY_VERSION_3, 999999, data3, _LINUX_CAPABILITY_U32S_3, EPERM) != 0) return -1;

    return 0;
}

int main() {
    int fails = 0;
    fails += (test_rule_effective_subset_permitted() != 0);
    fails += (test_rule_permitted_not_increase() != 0);
    fails += (test_rule_inheritable_bounds() != 0);
    fails += (test_version_paths() != 0);

    if (fails) {
        printf("test_sys_capset: %d test(s) failed\n", fails);
        return 1;
    }
    printf("test_sys_capset: all tests passed (note: some cases depend on initial cred defaults)\n");
    return 0;
}
