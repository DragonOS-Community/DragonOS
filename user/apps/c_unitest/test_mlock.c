#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_mlock2
#define SYS_mlock2 325
#endif

#ifndef MCL_ONFAULT
#define MCL_ONFAULT 4
#endif

static int g_total = 0;
static int g_failed = 0;

#define CHECK(cond, msg)                                                       \
    do {                                                                       \
        g_total++;                                                             \
        if (!(cond)) {                                                         \
            g_failed++;                                                        \
            fprintf(stderr, "FAIL: %s (line %d, errno=%d)\n", msg, __LINE__,   \
                    errno);                                                    \
        } else {                                                               \
            printf("PASS: %s\n", msg);                                         \
        }                                                                      \
    } while (0)

static size_t page_size(void) {
    long ps = sysconf(_SC_PAGESIZE);
    return ps > 0 ? (size_t)ps : 4096;
}

static void allow_memlock(size_t bytes) {
    struct rlimit lim = {
        .rlim_cur = bytes,
        .rlim_max = bytes,
    };
    (void)setrlimit(RLIMIT_MEMLOCK, &lim);
}

static void test_invalid_args(void) {
    size_t ps = page_size();
    char *addr = mmap(NULL, ps, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(addr != MAP_FAILED, "mmap for invalid arg tests");
    if (addr == MAP_FAILED) {
        return;
    }

    errno = 0;
    CHECK(mlock(addr, 0) == -1 && errno == EINVAL,
          "mlock length zero returns EINVAL");

    errno = 0;
    CHECK(munlock(addr, 0) == -1 && errno == EINVAL,
          "munlock length zero returns EINVAL");

    munmap(addr, ps);
}

static void test_permission_or_limit(void) {
    size_t ps = page_size();
    struct rlimit old_lim;
    int have_old = getrlimit(RLIMIT_MEMLOCK, &old_lim) == 0;
    struct rlimit zero = {
        .rlim_cur = 0,
        .rlim_max = have_old ? old_lim.rlim_max : 0,
    };

    char *addr = mmap(NULL, ps, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(addr != MAP_FAILED, "mmap for permission test");
    if (addr == MAP_FAILED) {
        return;
    }

    if (setrlimit(RLIMIT_MEMLOCK, &zero) == 0) {
        errno = 0;
        int ret = mlock(addr, ps);
        CHECK((ret == -1 && errno == EPERM) || ret == 0,
              "mlock with zero limit is denied or capability-bypassed");
        if (ret == 0) {
            CHECK(munlock(addr, ps) == 0, "munlock after capability-bypassed mlock");
        }
    }

    if (have_old) {
        (void)setrlimit(RLIMIT_MEMLOCK, &old_lim);
    }
    munmap(addr, ps);
}

static void test_anonymous_populate_and_unlock(void) {
    size_t ps = page_size();
    size_t len = ps * 2;
    unsigned char vec[2] = {0, 0};

    allow_memlock(len);
    char *addr = mmap(NULL, len, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(addr != MAP_FAILED, "mmap anonymous range");
    if (addr == MAP_FAILED) {
        return;
    }

    errno = 0;
    CHECK(mlock(addr + 1, ps) == 0, "mlock accepts unaligned range");
    CHECK(mincore(addr, len, vec) == 0, "mincore after mlock succeeds");
    CHECK((vec[0] & 1) && (vec[1] & 1), "mlock populated anonymous pages");
    CHECK(munlock(addr + 1, ps) == 0, "munlock accepts unaligned range");

    munmap(addr, len);
}

static void test_file_mapping_lock_lazy_fault(void) {
    size_t ps = page_size();
    char tmpl[] = "mlock_file_XXXXXX";
    int fd = mkstemp(tmpl);
    CHECK(fd >= 0, "create file for mlock");
    if (fd < 0) {
        return;
    }

    char *buf = malloc(ps);
    CHECK(buf != NULL, "allocate file buffer");
    if (!buf) {
        close(fd);
        unlink(tmpl);
        return;
    }
    memset(buf, 0x5a, ps);
    CHECK(write(fd, buf, ps) == (ssize_t)ps, "write file page");
    free(buf);

    char *addr = mmap(NULL, ps, PROT_READ, MAP_PRIVATE, fd, 0);
    CHECK(addr != MAP_FAILED, "mmap file range");
    if (addr == MAP_FAILED) {
        close(fd);
        unlink(tmpl);
        return;
    }

    allow_memlock(ps);
    CHECK(mlock(addr, ps) == 0, "mlock file mapping succeeds before fault");
    CHECK(addr[0] == 0x5a, "faulted locked file page is readable");
    CHECK(munlock(addr, ps) == 0, "munlock file mapping succeeds");

    munmap(addr, ps);
    close(fd);
    unlink(tmpl);
}

static void test_stubbed_locking_syscalls(void) {
    size_t ps = page_size();
    char *addr = mmap(NULL, ps, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(addr != MAP_FAILED, "mmap for stubbed locking syscalls");
    if (addr == MAP_FAILED) {
        return;
    }

    errno = 0;
    CHECK(syscall(SYS_mlock2, addr, 0, 0) == -1 && errno == EINVAL,
          "mlock2 length zero returns EINVAL");

    errno = 0;
    CHECK(syscall(SYS_mlock2, addr, ps, 0x80000000U) == -1 && errno == EINVAL,
          "mlock2 unknown flags return EINVAL");

    allow_memlock(ps);
    errno = 0;
    CHECK(syscall(SYS_mlock2, addr, ps, 0) == 0,
          "mlock2 valid flags returns success");

    errno = 0;
    CHECK(mlockall(0) == -1 && errno == EINVAL,
          "mlockall zero flags returns EINVAL");

    errno = 0;
    CHECK(mlockall(MCL_ONFAULT) == -1 && errno == EINVAL,
          "mlockall MCL_ONFAULT alone returns EINVAL");

    errno = 0;
    CHECK(mlockall(MCL_FUTURE | MCL_ONFAULT) == 0,
          "mlockall valid flags returns success");

    errno = 0;
    CHECK(munlockall() == 0, "munlockall returns success");

    munmap(addr, ps);
}

int main(void) {
    test_invalid_args();
    test_permission_or_limit();
    test_anonymous_populate_and_unlock();
    test_file_mapping_lock_lazy_fault();
    test_stubbed_locking_syscalls();

    printf("Summary: %d/%d passed\n", g_total - g_failed, g_total);
    return g_failed == 0 ? 0 : 1;
}
