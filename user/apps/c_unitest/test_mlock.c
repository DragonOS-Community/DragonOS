#define _GNU_SOURCE
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

#ifndef SYS_mremap
#define SYS_mremap 25
#endif

#ifndef MCL_ONFAULT
#define MCL_ONFAULT 4
#endif

#ifndef MREMAP_MAYMOVE
#define MREMAP_MAYMOVE 1
#endif

#ifndef MREMAP_DONTUNMAP
#define MREMAP_DONTUNMAP 4
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

static int raise_memlock_to_hard_limit(struct rlimit *old_lim) {
    if (getrlimit(RLIMIT_MEMLOCK, old_lim) != 0) {
        return 0;
    }

    struct rlimit lim = *old_lim;
    lim.rlim_cur = lim.rlim_max;
    return setrlimit(RLIMIT_MEMLOCK, &lim) == 0;
}

static void restore_memlock_limit(const struct rlimit *old_lim) {
    (void)setrlimit(RLIMIT_MEMLOCK, old_lim);
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

static void test_mlockall_current_semantics(void) {
    size_t ps = page_size();
    struct rlimit old_lim;
    if (!raise_memlock_to_hard_limit(&old_lim)) {
        printf("SKIP: unable to raise RLIMIT_MEMLOCK for mlockall current test\n");
        return;
    }

    char *guard = mmap(NULL, ps, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(guard != MAP_FAILED, "mmap PROT_NONE guard page");
    if (guard == MAP_FAILED) {
        restore_memlock_limit(&old_lim);
        return;
    }

    errno = 0;
    CHECK(mlockall(MCL_FUTURE) == 0, "mlockall MCL_FUTURE succeeds");

    errno = 0;
    if (mlockall(MCL_CURRENT) == -1 && errno == ENOMEM) {
        printf("SKIP: RLIMIT_MEMLOCK too small for mlockall(MCL_CURRENT) semantics test\n");
        (void)munlockall();
        munmap(guard, ps);
        restore_memlock_limit(&old_lim);
        return;
    }
    CHECK(errno == 0, "mlockall MCL_CURRENT succeeds with PROT_NONE VMA");

    char *fresh = mmap(NULL, ps, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(fresh != MAP_FAILED, "mmap after mlockall current");
    if (fresh != MAP_FAILED) {
        errno = 0;
        CHECK(msync(fresh, ps, MS_INVALIDATE) == 0,
              "mlockall MCL_CURRENT clears stale future locking");
        munmap(fresh, ps);
    }

    CHECK(munlockall() == 0, "munlockall after mlockall current semantics");
    munmap(guard, ps);
    restore_memlock_limit(&old_lim);
}

static void test_mremap_dontunmap_unlocks_source(void) {
    size_t ps = page_size();
    allow_memlock(ps * 4);
    char *addr = mmap(NULL, ps, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(addr != MAP_FAILED, "mmap for MREMAP_DONTUNMAP test");
    if (addr == MAP_FAILED) {
        return;
    }

    CHECK(mlock(addr, ps) == 0, "mlock source before MREMAP_DONTUNMAP");

    errno = 0;
    void *moved = (void *)syscall(SYS_mremap, addr, ps, ps,
                                  MREMAP_MAYMOVE | MREMAP_DONTUNMAP, 0);
    CHECK(moved != MAP_FAILED, "mremap MREMAP_DONTUNMAP succeeds");
    if (moved != MAP_FAILED) {
        errno = 0;
        CHECK(msync(addr, ps, MS_INVALIDATE) == 0,
              "source mapping is no longer locked after MREMAP_DONTUNMAP");
        errno = 0;
        CHECK(msync(moved, ps, MS_INVALIDATE) == -1 && errno == EBUSY,
              "destination mapping remains locked after MREMAP_DONTUNMAP");
        munmap(moved, ps);
    }

    munmap(addr, ps);
}

static void test_mremap_duplicate_unlocks_source(void) {
    size_t ps = page_size();
    allow_memlock(ps * 4);
    char *addr = mmap(NULL, ps, PROT_READ | PROT_WRITE,
                      MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(addr != MAP_FAILED, "mmap shared anonymous for duplicate mremap");
    if (addr == MAP_FAILED) {
        return;
    }

    CHECK(mlock(addr, ps) == 0, "mlock shared source before duplicate mremap");

    errno = 0;
    void *dup = (void *)syscall(SYS_mremap, addr, 0, ps, MREMAP_MAYMOVE);
    CHECK(dup != MAP_FAILED, "mremap old_len zero duplicate succeeds");
    if (dup != MAP_FAILED) {
        errno = 0;
        CHECK(msync(addr, ps, MS_INVALIDATE) == 0,
              "source mapping is no longer locked after duplicate mremap");
        errno = 0;
        CHECK(msync(dup, ps, MS_INVALIDATE) == -1 && errno == EBUSY,
              "duplicate mapping remains locked after old_len zero mremap");
        munmap(dup, ps);
    }

    munmap(addr, ps);
}

int main(void) {
    test_invalid_args();
    test_permission_or_limit();
    test_anonymous_populate_and_unlock();
    test_file_mapping_lock_lazy_fault();
    test_stubbed_locking_syscalls();
    test_mlockall_current_semantics();
    test_mremap_dontunmap_unlocks_source();
    test_mremap_duplicate_unlocks_source();

    printf("Summary: %d/%d passed\n", g_total - g_failed, g_total);
    return g_failed == 0 ? 0 : 1;
}
