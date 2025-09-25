#include <stdio.h>
#include <stdint.h>
#include <errno.h>
#include <string.h>
#include <unistd.h>
#include <sys/syscall.h>
#include <sys/wait.h>

/*
 * Inline minimal Linux capability ABI to avoid external headers:
 */
#define _LINUX_CAPABILITY_VERSION_1 0x19980330u
#define _LINUX_CAPABILITY_VERSION_2 0x20071026u /* deprecated */
#define _LINUX_CAPABILITY_VERSION_3 0x20080522u

#define _LINUX_CAPABILITY_U32S_1 1
#define _LINUX_CAPABILITY_U32S_2 2
#define _LINUX_CAPABILITY_U32S_3 2

typedef struct {
    uint32_t version;
    int32_t  pid;     /* target pid; 0 means current */
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

static int do_capget(uint32_t version, int32_t pid, cap_user_data_t *data, size_t elems, int expect_errno);

static int do_capset(uint32_t version, int32_t pid, cap_user_data_t *data, size_t elems) {
    // 仅用于子进程将自身能力清零：v3 两元素
    int ret = syscall(SYS_capset,
                      (cap_user_header_t[]){ { .version = version, .pid = pid } },
                      data);
    return ret;
}

static void fill_caps_v3(uint64_t e, uint64_t p, uint64_t i, cap_user_data_t out[2]) {
    out[0].effective   = (uint32_t)(e & 0xFFFFFFFFu);
    out[0].permitted   = (uint32_t)(p & 0xFFFFFFFFu);
    out[0].inheritable = (uint32_t)(i & 0xFFFFFFFFu);
    out[1].effective   = (uint32_t)((e >> 32) & 0xFFFFFFFFu);
    out[1].permitted   = (uint32_t)((p >> 32) & 0xFFFFFFFFu);
    out[1].inheritable = (uint32_t)((i >> 32) & 0xFFFFFFFFu);
}

// 新增用例：验证 pid!=0 时 capget 返回目标任务的 cred
static int test_capget_pid_nonzero() {
    pid_t child = fork();
    if (child < 0) {
        printf("[FAIL] fork failed: errno=%d(%s)\n", errno, strerror(errno));
        return -1;
    }
    if (child == 0) {
        // 子进程：将自身能力清零（允许降级）
        cap_user_data_t zeros[2];
        fill_caps_v3(0, 0, 0, zeros);
        if (do_capset(_LINUX_CAPABILITY_VERSION_3, 0, zeros, 2) != 0) {
            printf("[FAIL] child capset to zero failed: errno=%d(%s)\n", errno, strerror(errno));
            _exit(1);
        }
        // 给父进程留时间读取
        sleep(2);
        _exit(0);
    }
    // 父进程：稍等，读取子进程的能力
    sleep(1);
    cap_user_header_t hdr = { .version = _LINUX_CAPABILITY_VERSION_3, .pid = child };
    cap_user_data_t data[2] = {0};
    int ret = syscall(SYS_capget, &hdr, data);
    if (ret == -1) {
        printf("[FAIL] capget(pid=%d) syscall failed: errno=%d(%s)\n", child, errno, strerror(errno));
        int status;
        waitpid(child, &status, 0);
        return -1;
    }
    // 断言子进程的 e/p/i 全为 0
    if (data[0].effective != 0 || data[0].permitted != 0 || data[0].inheritable != 0 ||
        data[1].effective != 0 || data[1].permitted != 0 || data[1].inheritable != 0) {
        printf("[FAIL] capget(pid=%d) did not return zeros: "
               "eff=[0x%08x,0x%08x] per=[0x%08x,0x%08x] inh=[0x%08x,0x%08x]\n",
               child,
               data[0].effective, data[1].effective,
               data[0].permitted, data[1].permitted,
               data[0].inheritable, data[1].inheritable);
        int status;
        waitpid(child, &status, 0);
        return -1;
    }
    printf("[PASS] capget(pid=%d) returned zeros for child's capability sets\n", child);
    int status;
    waitpid(child, &status, 0);
    return 0;
}

static int do_capget(uint32_t version, int32_t pid, cap_user_data_t *data, size_t elems, int expect_errno)
{
    cap_user_header_t hdr;
    hdr.version = version;
    hdr.pid = pid;

    int ret = syscall(SYS_capget, &hdr, data);
    if (ret < 0) {
        int e = errno;
        if (expect_errno == 0) {
            printf("[FAIL] capget(version=0x%x,pid=%d) ret=%d errno=%d(%s), expected success\n",
                   version, pid, ret, e, strerror(e));
            return -1;
        }
        if (e != expect_errno) {
            printf("[FAIL] capget(version=0x%x,pid=%d) errno=%d(%s), expected %d(%s)\n",
                   version, pid, e, strerror(e), expect_errno, strerror(expect_errno));
            return -1;
        }
        printf("[PASS] capget(version=0x%x,pid=%d) failed as expected with errno=%d(%s)\n",
               version, pid, e, strerror(e));
        return 0;
    } else {
        if (expect_errno != 0) {
            printf("[FAIL] capget(version=0x%x,pid=%d) succeeded, expected errno=%d\n",
                   version, pid, expect_errno);
            return -1;
        }
        // Linux 预期：成功返回当前能力集；不断言具体值（非 root 通常为 0）
        printf("[PASS] capget(version=0x%x,pid=%d) succeeded: elements=%zu "
               "(eff=0x%08x/0x%08x per=0x%08x/0x%08x inh=0x%08x/0x%08x)\n",
               version, pid, elems,
               data[0].effective, elems>1?data[1].effective:0,
               data[0].permitted, elems>1?data[1].permitted:0,
               data[0].inheritable, elems>1?data[1].inheritable:0);
        return 0;
    }
}

static int test_v1_current(void)
{
    cap_user_data_t data[_LINUX_CAPABILITY_U32S_1] = {0};
    int r = do_capget(_LINUX_CAPABILITY_VERSION_1, 0, data, _LINUX_CAPABILITY_U32S_1, 0);
    if (r != 0) return -1;
    // 额外执行 pid!=0 测试
    return test_capget_pid_nonzero();
}

static int test_v2_current(void)
{
    cap_user_data_t data[_LINUX_CAPABILITY_U32S_2] = {0};
    return do_capget(_LINUX_CAPABILITY_VERSION_2, 0, data, _LINUX_CAPABILITY_U32S_2, 0);
}

static int test_v3_current(void)
{
    cap_user_data_t data[_LINUX_CAPABILITY_U32S_3] = {0};
    return do_capget(_LINUX_CAPABILITY_VERSION_3, 0, data, _LINUX_CAPABILITY_U32S_3, 0);
}

static int test_invalid_version_probe(void)
{
    cap_user_header_t hdr = {
        .version = 0xDEADBEEFu,
        .pid = 0,
    };
    // Probe: dataptr == NULL, expect ret==0 and hdr.version updated by kernel.
    int ret = syscall(SYS_capget, &hdr, NULL);
    if (ret < 0) {
        int e = errno;
        printf("[FAIL] probe capget(version=0x%x) ret=%d errno=%d(%s), expected success\n",
               0xDEADBEEFu, ret, e, strerror(e));
        return -1;
    }
    if (hdr.version != _LINUX_CAPABILITY_VERSION_3) {
        printf("[FAIL] probe updated version=0x%x, expected 0x%x\n",
               hdr.version, _LINUX_CAPABILITY_VERSION_3);
        return -1;
    }
    printf("[PASS] probe capget(version invalid) returned 0 and updated header.version to v3\n");
    return 0;
}

static int test_invalid_version_with_data(void)
{
    cap_user_data_t data[_LINUX_CAPABILITY_U32S_3] = {0};
    // Expect EINVAL when dataptr != NULL and version unknown
    return do_capget(0xCAFEBABEu, 0, data, _LINUX_CAPABILITY_U32S_3, EINVAL);
}

static int test_negative_pid(void)
{
    cap_user_data_t data[_LINUX_CAPABILITY_U32S_3] = {0};
    return do_capget(_LINUX_CAPABILITY_VERSION_3, -1, data, _LINUX_CAPABILITY_U32S_3, EINVAL);
}

static int test_null_dataptr_valid_version(void)
{
    cap_user_header_t hdr = {
        .version = _LINUX_CAPABILITY_VERSION_3,
        .pid = 0,
    };
    // Linux 观测：dataptr == NULL 且版本有效 → EINVAL 或某些实现返回 0
    int ret = syscall(SYS_capget, &hdr, NULL);
    if ((ret == -1 && errno == EINVAL) || (ret == 0)) {
        printf("[PASS] capget(dataptr=NULL, valid version) behaved as expected (ret=%d, errno=%d)\n",
               ret, errno);
        return 0;
    }
    printf("[FAIL] capget(dataptr=NULL, valid version) ret=%d errno=%d(%s), expected -1/EINVAL or 0\n",
           ret, errno, strerror(errno));
    return -1;
}

static int test_pid_not_exist(void)
{
    cap_user_data_t data[_LINUX_CAPABILITY_U32S_3] = {0};
    // Choose a large PID unlikely to exist (DragonOS should return ESRCH)
    return do_capget(_LINUX_CAPABILITY_VERSION_3, 999999, data, _LINUX_CAPABILITY_U32S_3, ESRCH);
}

int main(void)
{
    int fails = 0;

    fails += (test_v1_current() < 0);
    fails += (test_v2_current() < 0);
    fails += (test_v3_current() < 0);

    fails += (test_invalid_version_probe() < 0);
    fails += (test_invalid_version_with_data() < 0);

    fails += (test_negative_pid() < 0);
    fails += (test_null_dataptr_valid_version() < 0);
    fails += (test_pid_not_exist() < 0);

    // 额外：验证 pid!=0 的行为（比较父进程与子进程的能力集是否可读取）
    {
        pid_t child = fork();
        if (child == 0) {
            // 子进程不修改自身能力，直接退出
            _exit(0);
        } else if (child > 0) {
            cap_user_header_t hdr_self = { .version = _LINUX_CAPABILITY_VERSION_3, .pid = 0 };
            cap_user_data_t self_data[2] = {0};
            int r1 = syscall(SYS_capget, &hdr_self, self_data);

            cap_user_header_t hdr_child = { .version = _LINUX_CAPABILITY_VERSION_3, .pid = child };
            cap_user_data_t child_data[2] = {0};
            int r2 = syscall(SYS_capget, &hdr_child, child_data);

            int status;
            waitpid(child, &status, 0);

            if (r2 == -1) {
                printf("[FAIL] capget(pid=%d) failed: errno=%d(%s)\n", child, errno, strerror(errno));
                fails++;
            } else {
                // 不比较具体值，仅确认成功
                printf("[PASS] capget(pid=%d) succeeded\n", child);
            }
        } else {
            printf("[FAIL] fork failed: errno=%d(%s)\n", errno, strerror(errno));
            fails++;
        }
    }

    if (fails) {
        printf("test_sys_capget: %d test(s) failed\n", fails);
        return 1;
    }
    printf("test_sys_capget: all tests passed\n");
    return 0;
}