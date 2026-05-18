#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <unistd.h>

#ifndef __NR_sethostname
#if defined(__x86_64__)
#define __NR_sethostname 170
#elif defined(__riscv) || defined(__loongarch64)
#define __NR_sethostname 161
#else
#define __NR_sethostname 170
#endif
#endif

static inline int sys_sethostname(const char *name, size_t len)
{
    return syscall(__NR_sethostname, name, len);
}

static int expect_nodename(const char *expected)
{
    struct utsname uts;

    if (uname(&uts) != 0) {
        perror("uname failed");
        return 1;
    }

    if (strcmp(uts.nodename, expected) != 0) {
        printf("nodename mismatch: got '%s', expected '%s'\n", uts.nodename, expected);
        return 1;
    }

    return 0;
}

int main(void)
{
    struct utsname uts;
    char original[sizeof(uts.nodename)];
    char long_name[65];
    int failed = 0;

    printf("=== Testing sethostname syscall ===\n\n");

    if (uname(&uts) != 0) {
        perror("uname failed");
        return 1;
    }
    strncpy(original, uts.nodename, sizeof(original));
    original[sizeof(original) - 1] = '\0';
    printf("Initial hostname: '%s'\n\n", original);

    printf("Test 1: Set normal hostname\n");
    if (sys_sethostname("dragon-test", strlen("dragon-test")) == 0) {
        printf("sethostname succeeded\n");
        failed |= expect_nodename("dragon-test");
    } else {
        perror("sethostname failed");
        failed = 1;
    }
    printf("\n");

    printf("Test 2: Set empty hostname with len=0\n");
    if (sys_sethostname("", 0) == 0) {
        printf("sethostname accepted zero length\n");
        failed |= expect_nodename("");
    } else {
        perror("sethostname should accept zero length");
        failed = 1;
    }
    printf("\n");

    printf("Test 3: NULL pointer with len=0 should not fault\n");
    if (sys_sethostname(NULL, 0) == 0) {
        printf("sethostname accepted NULL with zero length\n");
        failed |= expect_nodename("");
    } else {
        perror("sethostname should accept NULL with zero length");
        failed = 1;
    }
    printf("\n");

    printf("Test 4: NULL pointer with non-zero length should fail with EFAULT\n");
    errno = 0;
    if (sys_sethostname(NULL, 1) == -1 && errno == EFAULT) {
        printf("sethostname correctly returned EFAULT\n");
    } else {
        printf("sethostname returned unexpected result, errno=%d\n", errno);
        failed = 1;
    }
    printf("\n");

    printf("Test 5: Too long hostname should fail with EINVAL\n");
    memset(long_name, 'a', sizeof(long_name));
    errno = 0;
    if (sys_sethostname(long_name, sizeof(long_name)) == -1 && errno == EINVAL) {
        printf("sethostname correctly returned EINVAL\n");
    } else {
        printf("sethostname returned unexpected result, errno=%d\n", errno);
        failed = 1;
    }
    printf("\n");

    printf("Cleanup: Restoring original hostname\n");
    if (sys_sethostname(original, strlen(original)) != 0) {
        perror("failed to restore hostname");
        failed = 1;
    }

    printf("\n=== Test %s ===\n", failed ? "failed" : "passed");
    return failed ? 1 : 0;
}
