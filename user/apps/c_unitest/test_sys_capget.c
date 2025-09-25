#include <stdio.h>
#include <stdint.h>
#include <errno.h>
#include <string.h>
#include <unistd.h>
#include <sys/syscall.h>

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
        // Validate returned capability sets are all-ones per DragonOS behavior
        for (size_t i = 0; i < elems; i++) {
            if (data[i].effective != 0xFFFFFFFFu ||
                data[i].permitted != 0xFFFFFFFFu ||
                data[i].inheritable != 0xFFFFFFFFu) {
                printf("[FAIL] capget(version=0x%x,index=%zu) values not all-ones: eff=0x%08x per=0x%08x inh=0x%08x\n",
                       version, i, data[i].effective, data[i].permitted, data[i].inheritable);
                return -1;
            }
        }
        printf("[PASS] capget(version=0x%x,pid=%d) returned %zu elements with all-ones capability sets\n",
               version, pid, elems);
        return 0;
    }
}

static int test_v1_current(void)
{
    cap_user_data_t data[_LINUX_CAPABILITY_U32S_1] = {0};
    return do_capget(_LINUX_CAPABILITY_VERSION_1, 0, data, _LINUX_CAPABILITY_U32S_1, 0);
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
    // dataptr == NULL with valid version should be EFAULT
    int ret = syscall(SYS_capget, &hdr, NULL);
    if (ret == -1 && errno == EFAULT) {
        printf("[PASS] capget(dataptr=NULL, valid version) failed with EFAULT as expected\n");
        return 0;
    }
    printf("[FAIL] capget(dataptr=NULL, valid version) ret=%d errno=%d(%s), expected -1/EFAULT\n",
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

    if (fails) {
        printf("test_sys_capget: %d test(s) failed\n", fails);
        return 1;
    }
    printf("test_sys_capget: all tests passed\n");
    return 0;
}