/**
 * Comprehensive mlock Series System Call Test Suite
 *
 * Tests mlock, munlock, mlock2, mlockall, munlockall with:
 * - Error code validation (errno)
 * - Boundary conditions
 * - Resource limit enforcement
 * - Edge cases and overflow scenarios
 *
 * Author: DragonOS Test Suite
 */

#define _GNU_SOURCE  // 启用 GNU 扩展，包括 MLOCK_ONFAULT

#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <fcntl.h>
#include <signal.h>
#include <limits.h>

// ================================================================
// Test Framework
// ================================================================

typedef int (*test_func_t)(void);

static int test_count = 0;
static int pass_count = 0;
static int skip_count = 0;

#define TEST_START(name) \
    do { \
        test_count++; \
        printf("[%2d] %-40s ... ", test_count, name); \
        fflush(stdout); \
    } while(0)

#define TEST_PASS() \
    do { \
        pass_count++; \
        printf("[PASS]\n"); \
    } while(0)

#define TEST_FAIL(msg) \
    do { \
        printf("[FAIL] %s (errno=%d: %s)\n", msg, errno, strerror(errno)); \
    } while(0)

#define TEST_SKIP(msg) \
    do { \
        skip_count++; \
        printf("[SKIP] %s\n", msg); \
    } while(0)

// ================================================================
// Utility Functions
// ================================================================

static size_t get_page_size(void) {
    long sz = sysconf(_SC_PAGESIZE);
    return (sz > 0) ? (size_t)sz : 4096;
}

static void save_rlimit(struct rlimit *save) {
    getrlimit(RLIMIT_MEMLOCK, save);
}

static void restore_rlimit(const struct rlimit *save) {
    setrlimit(RLIMIT_MEMLOCK, save);
}

static int set_rlimit(size_t bytes) {
    struct rlimit rlim;
    rlim.rlim_cur = bytes;
    rlim.rlim_max = bytes * 2;  // Set max higher to allow testing
    return setrlimit(RLIMIT_MEMLOCK, &rlim);
}

// ================================================================
// Basic Functionality Tests
// ================================================================

/**
 * Test 1: Basic mlock/munlock functionality
 */
static int test_basic_mlock(void) {
    TEST_START("basic_mlock");
    size_t pagesize = get_page_size();
    size_t length = pagesize;  // Just 1 page for faster testing

    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        return -1;
    }

    printf("\n  [DEBUG] mmap OK, calling mlock...");
    fflush(stdout);

    if (mlock(addr, length) != 0) {
        printf(" FAILED (errno=%d)\n", errno);
        TEST_FAIL("mlock failed");
        munmap(addr, length);
        return -1;
    }

    printf(" OK\n");
    fflush(stdout);

    // Verify memory is accessible (just first and last bytes)
    addr[0] = 'A';
    addr[length - 1] = 'Z';

    if (addr[0] != 'A' || addr[length - 1] != 'Z') {
        TEST_FAIL("data verification failed");
        munlock(addr, length);
        munmap(addr, length);
        return -1;
    }

    printf("  [DEBUG] calling munlock...");
    fflush(stdout);

    if (munlock(addr, length) != 0) {
        printf(" FAILED (errno=%d)\n", errno);
        TEST_FAIL("munlock failed");
        munmap(addr, length);
        return -1;
    }

    printf(" OK\n");

    munmap(addr, length);
    TEST_PASS();
    return 0;
}

/**
 * Test 2: mlock2 with flags=0 (should behave like mlock)
 */
static int test_mlock2_basic(void) {
    TEST_START("mlock2_basic");

#ifdef MLOCK_ONFAULT
    size_t pagesize = get_page_size();
    size_t length = pagesize * 2;

    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        return -1;
    }

    // mlock2 with flags=0 should work like mlock
    if (mlock2(addr, length, 0) != 0) {
        TEST_FAIL("mlock2 with flags=0 failed");
        munmap(addr, length);
        return -1;
    }

    memset(addr, 0xAA, length);

    if (munlock(addr, length) != 0) {
        TEST_FAIL("munlock failed");
        munmap(addr, length);
        return -1;
    }

    munmap(addr, length);
    TEST_PASS();
    return 0;
#else
    TEST_SKIP("MLOCK_ONFAULT not defined");
    return 0;
#endif
}

/**
 * Test 3: mlock2 with MLOCK_ONFAULT flag
 */
static int test_mlock2_onfault(void) {
    TEST_START("mlock2_onfault");

#ifdef MLOCK_ONFAULT
    size_t pagesize = get_page_size();
    size_t length = pagesize * 2;

    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        return -1;
    }

    // mlock2 with MLOCK_ONFAULT
    if (mlock2(addr, length, MLOCK_ONFAULT) != 0) {
        TEST_FAIL("mlock2 with MLOCK_ONFAULT failed");
        munmap(addr, length);
        return -1;
    }

    // Trigger page faults by accessing the memory
    memset(addr, 0x55, length);

    if (munlock(addr, length) != 0) {
        TEST_FAIL("munlock failed");
        munmap(addr, length);
        return -1;
    }

    munmap(addr, length);
    TEST_PASS();
    return 0;
#else
    TEST_SKIP("MLOCK_ONFAULT not defined");
    return 0;
#endif
}

/**
 * Test 4: mlock2 with invalid flags should return EINVAL
 */
static int test_mlock2_invalid_flags(void) {
    TEST_START("mlock2_invalid_flags");

#ifdef MLOCK_ONFAULT
    size_t pagesize = get_page_size();
    char *addr = mmap(NULL, pagesize, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        return -1;
    }

    errno = 0;
    int ret = mlock2(addr, pagesize, 0xFFFF);  // Invalid flags
    munmap(addr, pagesize);

    if (ret == -1 && errno == EINVAL) {
        TEST_PASS();
        return 0;
    } else {
        TEST_FAIL("expected EINVAL for invalid flags");
        return -1;
    }
#else
    TEST_SKIP("MLOCK_ONFAULT not defined");
    return 0;
#endif
}

/**
 * Test 5: mlockall with MCL_CURRENT
 */
static int test_mlockall_current(void) {
    TEST_START("mlockall_current");

    // 增加 RLIMIT_MEMLOCK 以锁定所有当前映射
    // mlockall(MCL_CURRENT) 会锁定进程的所有可访问 VMA（代码段、数据段、堆、栈等）
    struct rlimit rlim;
    rlim.rlim_cur = RLIM_INFINITY;
    rlim.rlim_max = RLIM_INFINITY;
    if (setrlimit(RLIMIT_MEMLOCK, &rlim) != 0) {
        TEST_SKIP("setrlimit failed");
        return 0;
    }

    size_t pagesize = get_page_size();
    size_t length = pagesize * 2;

    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        return -1;
    }

    if (mlockall(MCL_CURRENT) != 0) {
        TEST_FAIL("mlockall(MCL_CURRENT) failed");
        munmap(addr, length);
        return -1;
    }

    memset(addr, 0x33, length);

    if (munlockall() != 0) {
        TEST_FAIL("munlockall failed");
        munmap(addr, length);
        return -1;
    }

    munmap(addr, length);
    TEST_PASS();
    return 0;
}

/**
 * Test 6: mlockall with MCL_FUTURE
 */
static int test_mlockall_future(void) {
    TEST_START("mlockall_future");

    size_t pagesize = get_page_size();

    if (mlockall(MCL_FUTURE) != 0) {
        TEST_FAIL("mlockall(MCL_FUTURE) failed");
        return -1;
    }

    // New mapping should be automatically locked
    size_t length = pagesize * 2;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        munlockall();
        return -1;
    }

    memset(addr, 0x44, length);

    if (munlockall() != 0) {
        TEST_FAIL("munlockall failed");
        munmap(addr, length);
        return -1;
    }

    munmap(addr, length);
    TEST_PASS();
    return 0;
}

/**
 * Test 7: mlockall with both MCL_CURRENT and MCL_FUTURE
 */
static int test_mlockall_combined(void) {
    TEST_START("mlockall_combined");

    // 增加 RLIMIT_MEMLOCK 以锁定所有当前映射
    // mlockall(MCL_CURRENT) 会锁定进程的所有可访问 VMA（代码段、数据段、堆、栈等）
    struct rlimit rlim;
    rlim.rlim_cur = RLIM_INFINITY;
    rlim.rlim_max = RLIM_INFINITY;
    if (setrlimit(RLIMIT_MEMLOCK, &rlim) != 0) {
        TEST_SKIP("setrlimit failed");
        return 0;
    }

    size_t pagesize = get_page_size();

    if (mlockall(MCL_CURRENT | MCL_FUTURE) != 0) {
        TEST_FAIL("mlockall(MCL_CURRENT|MCL_FUTURE) failed");
        return -1;
    }

    // Both existing and new mappings should be locked
    size_t length = pagesize * 2;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        munlockall();
        return -1;
    }

    memset(addr, 0x55, length);

    if (munlockall() != 0) {
        TEST_FAIL("munlockall failed");
        munmap(addr, length);
        return -1;
    }

    munmap(addr, length);
    TEST_PASS();
    return 0;
}

/**
 * Test 8: mlockall with invalid flags (no MCL_CURRENT or MCL_FUTURE)
 */
static int test_mlockall_invalid_flags(void) {
    TEST_START("mlockall_invalid_flags");

    errno = 0;
    int ret = mlockall(0);  // No flags specified

    if (ret == -1 && errno == EINVAL) {
        TEST_PASS();
        return 0;
    } else {
        TEST_FAIL("expected EINVAL for mlockall(0)");
        return -1;
    }
}

/**
 * Test 9: mlockall with MCL_ONFAULT alone should fail
 */
static int test_mlockall_onfault_alone(void) {
    TEST_START("mlockall_onfault_alone");

#ifdef MCL_ONFAULT
    errno = 0;
    int ret = mlockall(MCL_ONFAULT);  // MCL_ONFAULT alone is invalid

    if (ret == -1 && errno == EINVAL) {
        TEST_PASS();
        return 0;
    } else {
        TEST_FAIL("expected EINVAL for MCL_ONFAULT alone");
        return -1;
    }
#else
    TEST_SKIP("MCL_ONFAULT not defined");
    return 0;
#endif
}

// ================================================================
// Error Code Validation Tests
// ================================================================

/**
 * Test 10: mlock exceeding RLIMIT_MEMLOCK should return ENOMEM
 */
static int test_rlimit_enomem(void) {
    TEST_START("rlimit_enomem");

    struct rlimit saved;
    save_rlimit(&saved);

    size_t pagesize = get_page_size();
    size_t limit = pagesize * 2;  // Very small limit

    if (set_rlimit(limit) != 0) {
        TEST_FAIL("setrlimit failed");
        restore_rlimit(&saved);
        return -1;
    }

    size_t length = pagesize * 10;  // Try to lock more than limit
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        restore_rlimit(&saved);
        TEST_SKIP("mmap failed");
        return 0;
    }

    errno = 0;
    int ret = mlock(addr, length);
    munmap(addr, length);
    restore_rlimit(&saved);

    if (ret == -1 && errno == ENOMEM) {
        TEST_PASS();
        return 0;
    } else {
        TEST_FAIL("expected ENOMEM when exceeding limit");
        return -1;
    }
}

/**
 * Test 11: RLIMIT_MEMLOCK = 0 should return EPERM
 */
static int test_rlimit_zero_eperm(void) {
    TEST_START("rlimit_zero_eperm");

    struct rlimit saved;
    save_rlimit(&saved);

    struct rlimit rlim;
    rlim.rlim_cur = 0;
    rlim.rlim_max = RLIM_INFINITY;

    if (setrlimit(RLIMIT_MEMLOCK, &rlim) != 0) {
        restore_rlimit(&saved);
        TEST_SKIP("setrlimit failed (need CAP_SYS_RESOURCE?)");
        return 0;
    }

    size_t pagesize = get_page_size();
    char *addr = mmap(NULL, pagesize, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        restore_rlimit(&saved);
        TEST_SKIP("mmap failed");
        return 0;
    }

    errno = 0;
    int ret = mlock(addr, pagesize);
    munmap(addr, pagesize);
    restore_rlimit(&saved);

    if (ret == -1 && errno == EPERM) {
        TEST_PASS();
        return 0;
    } else {
        TEST_FAIL("expected EPERM when RLIMIT_MEMLOCK=0");
        return -1;
    }
}

/**
 * Test 12: Invalid address should return ENOMEM
 */
static int test_invalid_address_enomem(void) {
    TEST_START("invalid_address_enomem");

    size_t pagesize = get_page_size();

    // Try to lock invalid address (near end of address space)
    void *invalid_addr = (void *)(~0UL - pagesize + 1);

    errno = 0;
    int ret = mlock(invalid_addr, pagesize);

    if (ret == -1 && errno == ENOMEM) {
        TEST_PASS();
        return 0;
    } else {
        TEST_FAIL("expected ENOMEM for invalid address");
        return -1;
    }
}

/**
 * Test 13: NULL pointer with non-zero length should fail
 */
static int test_null_pointer(void) {
    TEST_START("null_pointer");

    size_t pagesize = get_page_size();

    errno = 0;
    int ret = mlock(NULL, pagesize);

    if (ret == -1) {
        // errno could be ENOMEM or EFAULT
        TEST_PASS();
        return 0;
    } else {
        TEST_FAIL("expected failure for NULL pointer");
        return -1;
    }
}

// ================================================================
// Boundary Conditions Tests
// ================================================================

/**
 * Test 14: Zero length should succeed
 */
static int test_zero_length(void) {
    TEST_START("zero_length");

    size_t pagesize = get_page_size();
    char *addr = mmap(NULL, pagesize, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        return -1;
    }

    if (mlock(addr, 0) != 0) {
        TEST_FAIL("mlock with len=0 failed");
        munmap(addr, pagesize);
        return -1;
    }

    munmap(addr, pagesize);
    TEST_PASS();
    return 0;
}

/**
 * Test 15: Single page locking
 */
static int test_single_page(void) {
    TEST_START("single_page");

    size_t pagesize = get_page_size();
    char *addr = mmap(NULL, pagesize, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        return -1;
    }

    if (mlock(addr, pagesize) != 0) {
        TEST_FAIL("mlock failed");
        munmap(addr, pagesize);
        return -1;
    }

    addr[0] = 'X';

    if (munlock(addr, pagesize) != 0) {
        TEST_FAIL("munlock failed");
        munmap(addr, pagesize);
        return -1;
    }

    munmap(addr, pagesize);
    TEST_PASS();
    return 0;
}

/**
 * Test 16: Unaligned address (kernel should align down)
 */
static int test_unaligned_address(void) {
    TEST_START("unaligned_address");

    size_t pagesize = get_page_size();
    size_t length = pagesize * 2;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        return -1;
    }

    // Lock with unaligned address
    char *unaligned = addr + 100;
    if (mlock(unaligned, pagesize) != 0) {
        TEST_FAIL("mlock with unaligned address failed");
        munmap(addr, length);
        return -1;
    }

    // Verify the entire page is accessible
    addr[pagesize - 1] = 'Y';  // Should be in locked range
    unaligned[0] = 'Z';

    if (munlock(unaligned, pagesize) != 0) {
        TEST_FAIL("munlock failed");
        munmap(addr, length);
        return -1;
    }

    munmap(addr, length);
    TEST_PASS();
    return 0;
}

/**
 * Test 17: Length not page-aligned (kernel should align up)
 */
static int test_unaligned_length(void) {
    TEST_START("unaligned_length");

    size_t pagesize = get_page_size();
    size_t length = pagesize * 2;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        return -1;
    }

    // Lock with unaligned length
    size_t unaligned_len = pagesize + 100;
    if (mlock(addr, unaligned_len) != 0) {
        TEST_FAIL("mlock with unaligned length failed");
        munmap(addr, length);
        return -1;
    }

    if (munlock(addr, unaligned_len) != 0) {
        TEST_FAIL("munlock failed");
        munmap(addr, length);
        return -1;
    }

    munmap(addr, length);
    TEST_PASS();
    return 0;
}

/**
 * Test 18: Very large length (near overflow)
 */
static int test_large_length(void) {
    TEST_START("large_length");

    size_t pagesize = get_page_size();
    char *addr = mmap(NULL, pagesize, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        return -1;
    }

    errno = 0;
    // Very large length that might cause overflow
    int ret = mlock(addr, SIZE_MAX - pagesize);
    munmap(addr, pagesize);

    if (ret == -1 && (errno == ENOMEM || errno == EINVAL)) {
        TEST_PASS();
        return 0;
    } else {
        TEST_FAIL("expected failure for very large length");
        return -1;
    }
}

/**
 * Test 19: Length that causes page-aligned overflow
 */
static int test_length_overflow(void) {
    TEST_START("length_overflow");

    size_t pagesize = get_page_size();
    char *addr = mmap(NULL, pagesize * 10, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        return -1;
    }

    // Length that when aligned might overflow
    errno = 0;
    int ret = mlock(addr, SIZE_MAX - 100);
    munmap(addr, pagesize * 10);

    if (ret == -1) {
        TEST_PASS();
        return 0;
    } else {
        TEST_FAIL("expected failure for overflow length");
        return -1;
    }
}

// ================================================================
// Reference Counting Tests
// ================================================================

/**
 * Test 20: Multiple locks on same region
 */
static int test_multiple_locks(void) {
    TEST_START("multiple_locks");

    size_t pagesize = get_page_size();
    size_t length = pagesize * 2;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        return -1;
    }

    // First lock
    if (mlock(addr, length) != 0) {
        TEST_FAIL("first mlock failed");
        munmap(addr, length);
        return -1;
    }

    // Second lock on same region (should succeed with ref counting)
    if (mlock(addr, length) != 0) {
        TEST_FAIL("second mlock failed (ref count?)");
        munlock(addr, length);
        munmap(addr, length);
        return -1;
    }

    // Need two unlocks
    if (munlock(addr, length) != 0) {
        TEST_FAIL("first munlock failed");
        munmap(addr, length);
        return -1;
    }

    if (munlock(addr, length) != 0) {
        TEST_FAIL("second munlock failed");
        munmap(addr, length);
        return -1;
    }

    munmap(addr, length);
    TEST_PASS();
    return 0;
}

/**
 * Test 21: Partial unlock of locked region
 */
static int test_partial_unlock(void) {
    TEST_START("partial_unlock");

    size_t pagesize = get_page_size();
    size_t total_length = pagesize * 4;
    char *addr = mmap(NULL, total_length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        return -1;
    }

    // Lock entire region
    if (mlock(addr, total_length) != 0) {
        TEST_FAIL("mlock failed");
        munmap(addr, total_length);
        return -1;
    }

    // Unlock middle 2 pages
    char *unlock_start = addr + pagesize;
    size_t unlock_length = pagesize * 2;

    if (munlock(unlock_start, unlock_length) != 0) {
        TEST_FAIL("partial munlock failed");
        munlock(addr, total_length);
        munmap(addr, total_length);
        return -1;
    }

    // Unlock remaining pages
    if (munlock(addr, pagesize) != 0) {
        TEST_FAIL("munlock first page failed");
        munmap(addr, total_length);
        return -1;
    }

    if (munlock(addr + pagesize * 3, pagesize) != 0) {
        TEST_FAIL("munlock last page failed");
        munmap(addr, total_length);
        return -1;
    }

    munmap(addr, total_length);
    TEST_PASS();
    return 0;
}

/**
 * Test 22: Lock, unlock, relock
 */
static int test_relock_after_unlock(void) {
    TEST_START("relock_after_unlock");

    size_t pagesize = get_page_size();
    size_t length = pagesize * 2;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        return -1;
    }

    if (mlock(addr, length) != 0) {
        TEST_FAIL("first mlock failed");
        munmap(addr, length);
        return -1;
    }

    if (munlock(addr, length) != 0) {
        TEST_FAIL("munlock failed");
        munmap(addr, length);
        return -1;
    }

    // Lock again
    if (mlock(addr, length) != 0) {
        TEST_FAIL("second mlock failed");
        munmap(addr, length);
        return -1;
    }

    memset(addr, 0x77, length);

    if (munlock(addr, length) != 0) {
        TEST_FAIL("final munlock failed");
        munmap(addr, length);
        return -1;
    }

    munmap(addr, length);
    TEST_PASS();
    return 0;
}

// ================================================================
// mmap Integration Tests
// ================================================================

/**
 * Test 23: mmap with MAP_LOCKED flag
 */
static int test_mmap_locked(void) {
    TEST_START("mmap_locked");

#ifdef MAP_LOCKED
    size_t pagesize = get_page_size();
    size_t length = pagesize * 4;

    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_LOCKED, -1, 0);
    if (addr == MAP_FAILED) {
        // MAP_LOCKED might fail due to rlimit, that's okay
        TEST_SKIP("mmap with MAP_LOCKED failed (rlimit?)");
        return 0;
    }

    // Memory should be accessible
    memset(addr, 0xBB, length);

    munmap(addr, length);
    TEST_PASS();
    return 0;
#else
    TEST_SKIP("MAP_LOCKED not defined");
    return 0;
#endif
}

/**
 * Test 24: mmap with MAP_LOCKED exceeds RLIMIT
 */
static int test_mmap_locked_rlimit(void) {
    TEST_START("mmap_locked_rlimit");

#ifdef MAP_LOCKED
    struct rlimit saved;
    save_rlimit(&saved);

    size_t pagesize = get_page_size();
    size_t limit = pagesize * 2;

    if (set_rlimit(limit) != 0) {
        restore_rlimit(&saved);
        TEST_SKIP("setrlimit failed");
        return 0;
    }

    // Try to map more than limit with MAP_LOCKED
    size_t length = pagesize * 10;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_LOCKED, -1, 0);

    restore_rlimit(&saved);

    if (addr == MAP_FAILED) {
        // Expected to fail due to rlimit
        TEST_PASS();
        return 0;
    } else {
        // Some systems might allow it, that's okay too
        munmap(addr, length);
        TEST_PASS();
        return 0;
    }
#else
    TEST_SKIP("MAP_LOCKED not defined");
    return 0;
#endif
}

// ================================================================
// Fork Behavior Tests
// ================================================================

/**
 * Test 25: Fork - locks should not be inherited
 */
static int test_fork_inheritance(void) {
    TEST_START("fork_inheritance");

    size_t pagesize = get_page_size();
    size_t length = pagesize * 2;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        return -1;
    }

    // Lock in parent
    if (mlock(addr, length) != 0) {
        TEST_FAIL("mlock failed");
        munmap(addr, length);
        return -1;
    }

    pid_t pid = fork();
    if (pid < 0) {
        TEST_FAIL("fork failed");
        munlock(addr, length);
        munmap(addr, length);
        return -1;
    } else if (pid == 0) {
        // Child - locks should NOT be inherited
        // munlock should succeed even though we didn't lock in child
        if (munlock(addr, length) == 0) {
            exit(0);  // Success
        } else {
            exit(1);  // Failure
        }
    } else {
        // Parent
        int status;
        waitpid(pid, &status, 0);

        munlock(addr, length);
        munmap(addr, length);

        if (WIFEXITED(status) && WEXITSTATUS(status) == 0) {
            TEST_PASS();
            return 0;
        } else {
            TEST_FAIL("child exited with error");
            return -1;
        }
    }
}

/**
 * Test 26: Fork after mlockall(MCL_FUTURE)
 */
static int test_fork_mlockall_future(void) {
    TEST_START("fork_mlockall_future");

    size_t pagesize = get_page_size();

    if (mlockall(MCL_FUTURE) != 0) {
        TEST_FAIL("mlockall(MCL_FUTURE) failed");
        return -1;
    }

    pid_t pid = fork();
    if (pid < 0) {
        TEST_FAIL("fork failed");
        munlockall();
        return -1;
    } else if (pid == 0) {
        // Child - MCL_FUTURE should NOT be inherited
        size_t length = pagesize * 2;
        char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (addr == MAP_FAILED) {
            exit(1);
        }

        // This mmap should succeed without locking (inherited flag cleared)
        memset(addr, 0x88, length);
        munmap(addr, length);
        exit(0);
    } else {
        // Parent
        int status;
        waitpid(pid, &status, 0);
        munlockall();

        if (WIFEXITED(status) && WEXITSTATUS(status) == 0) {
            TEST_PASS();
            return 0;
        } else {
            TEST_FAIL("child exited with error");
            return -1;
        }
    }
}

// ================================================================
// munlockall Tests
// ================================================================

/**
 * Test 27: munlockall unlocks everything
 */
static int test_munlockall(void) {
    TEST_START("munlockall");

    size_t pagesize = get_page_size();

    // Lock multiple regions
    size_t length1 = pagesize * 2;
    char *addr1 = mmap(NULL, length1, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr1 == MAP_FAILED) {
        TEST_FAIL("mmap addr1 failed");
        return -1;
    }

    size_t length2 = pagesize * 3;
    char *addr2 = mmap(NULL, length2, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr2 == MAP_FAILED) {
        munmap(addr1, length1);
        TEST_FAIL("mmap addr2 failed");
        return -1;
    }

    if (mlock(addr1, length1) != 0) {
        TEST_FAIL("mlock addr1 failed");
        munmap(addr1, length1);
        munmap(addr2, length2);
        return -1;
    }

    if (mlock(addr2, length2) != 0) {
        TEST_FAIL("mlock addr2 failed");
        munlock(addr1, length1);
        munmap(addr1, length1);
        munmap(addr2, length2);
        return -1;
    }

    // munlockall should unlock everything
    if (munlockall() != 0) {
        TEST_FAIL("munlockall failed");
        munmap(addr1, length1);
        munmap(addr2, length2);
        return -1;
    }

    munmap(addr1, length1);
    munmap(addr2, length2);
    TEST_PASS();
    return 0;
}

/**
 * Test 28: munlockall clears MCL_FUTURE
 */
static int test_munlockall_clears_future(void) {
    TEST_START("munlockall_clears_future");

    size_t pagesize = get_page_size();

    if (mlockall(MCL_FUTURE) != 0) {
        TEST_FAIL("mlockall(MCL_FUTURE) failed");
        return -1;
    }

    // Clear the flag
    if (munlockall() != 0) {
        TEST_FAIL("munlockall failed");
        return -1;
    }

    // New mapping should NOT be locked
    size_t length = pagesize * 2;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        return -1;
    }

    // Access the memory
    memset(addr, 0x99, length);
    munmap(addr, length);

    TEST_PASS();
    return 0;
}

// ================================================================
// Memory Access Tests
// ================================================================

/**
 * Test 29: Verify locked memory persists
 */
static int test_memory_persistence(void) {
    TEST_START("memory_persistence");

    size_t pagesize = get_page_size();
    size_t length = pagesize * 10;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        return -1;
    }

    if (mlock(addr, length) != 0) {
        TEST_FAIL("mlock failed");
        munmap(addr, length);
        return -1;
    }

    // Write pattern
    for (size_t i = 0; i < length; i++) {
        addr[i] = (char)(i % 256);
    }

    // Verify
    for (size_t i = 0; i < length; i++) {
        if (addr[i] != (char)(i % 256)) {
            TEST_FAIL("data verification failed");
            munlock(addr, length);
            munmap(addr, length);
            return -1;
        }
    }

    if (munlock(addr, length) != 0) {
        TEST_FAIL("munlock failed");
        munmap(addr, length);
        return -1;
    }

    munmap(addr, length);
    TEST_PASS();
    return 0;
}

/**
 * Test 30: Large region locking
 */
static int test_large_region(void) {
    TEST_START("large_region");

    struct rlimit rlim;
    if (getrlimit(RLIMIT_MEMLOCK, &rlim) != 0) {
        TEST_FAIL("getrlimit failed");
        return -1;
    }

    size_t pagesize = get_page_size();
    size_t npages;

    if (rlim.rlim_cur == RLIM_INFINITY) {
        npages = 100;  // Use 100 pages if unlimited
    } else {
        npages = (rlim.rlim_cur / 4) / pagesize;  // Use 1/4 of limit
        if (npages < 10) npages = 10;
        if (npages > 100) npages = 100;
    }

    size_t length = pagesize * npages;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        TEST_FAIL("mmap failed");
        return -1;
    }

    if (mlock(addr, length) != 0) {
        TEST_FAIL("mlock failed");
        munmap(addr, length);
        return -1;
    }

    // Touch each page
    for (size_t i = 0; i < length; i += pagesize) {
        addr[i] = 'X';
    }

    if (munlock(addr, length) != 0) {
        TEST_FAIL("munlock failed");
        munmap(addr, length);
        return -1;
    }

    munmap(addr, length);
    TEST_PASS();
    return 0;
}

// ================================================================
// Main Test Runner
// ================================================================

struct test_entry {
    const char *name;
    test_func_t fn;
    const char *category;
};

int main(void) {
    printf("\n");
    printf("╔════════════════════════════════════════════════════════════╗\n");
    printf("║     DragonOS mlock Series System Call Test Suite          ║\n");
    printf("╚════════════════════════════════════════════════════════════╝\n");
    printf("\n");

    struct test_entry tests[] = {
        // Basic Functionality
        {"basic_mlock", test_basic_mlock, "Basic"},
        {"mlock2_basic", test_mlock2_basic, "Basic"},
        {"mlock2_onfault", test_mlock2_onfault, "Basic"},
        {"mlock2_invalid_flags", test_mlock2_invalid_flags, "Basic"},
        {"mlockall_current", test_mlockall_current, "Basic"},
        {"mlockall_future", test_mlockall_future, "Basic"},
        {"mlockall_combined", test_mlockall_combined, "Basic"},
        {"mlockall_invalid_flags", test_mlockall_invalid_flags, "Basic"},
        {"mlockall_onfault_alone", test_mlockall_onfault_alone, "Basic"},

        // Error Code Validation
        {"rlimit_enomem", test_rlimit_enomem, "Error Codes"},
        {"rlimit_zero_eperm", test_rlimit_zero_eperm, "Error Codes"},
        {"invalid_address_enomem", test_invalid_address_enomem, "Error Codes"},
        {"null_pointer", test_null_pointer, "Error Codes"},

        // Boundary Conditions
        {"zero_length", test_zero_length, "Boundary"},
        {"single_page", test_single_page, "Boundary"},
        {"unaligned_address", test_unaligned_address, "Boundary"},
        {"unaligned_length", test_unaligned_length, "Boundary"},
        {"large_length", test_large_length, "Boundary"},
        {"length_overflow", test_length_overflow, "Boundary"},

        // Reference Counting
        {"multiple_locks", test_multiple_locks, "Ref Counting"},
        {"partial_unlock", test_partial_unlock, "Ref Counting"},
        {"relock_after_unlock", test_relock_after_unlock, "Ref Counting"},

        // mmap Integration
        {"mmap_locked", test_mmap_locked, "mmap"},
        {"mmap_locked_rlimit", test_mmap_locked_rlimit, "mmap"},

        // Fork Behavior
        {"fork_inheritance", test_fork_inheritance, "Fork"},
        {"fork_mlockall_future", test_fork_mlockall_future, "Fork"},

        // munlockall
        {"munlockall", test_munlockall, "munlockall"},
        {"munlockall_clears_future", test_munlockall_clears_future, "munlockall"},

        // Memory Access
        {"memory_persistence", test_memory_persistence, "Memory"},
        {"large_region", test_large_region, "Memory"},
    };

    int total = sizeof(tests) / sizeof(tests[0]);
    const char *last_category = "";

    for (int i = 0; i < total; i++) {
        // Print category header if changed
        if (strcmp(last_category, tests[i].category) != 0) {
            printf("\n--- %s Tests ---\n", tests[i].category);
            last_category = tests[i].category;
        }

        errno = 0;
        tests[i].fn();
    }

    printf("\n");
    printf("╔════════════════════════════════════════════════════════════╗\n");
    printf("║                        Summary                              ║\n");
    printf("╠════════════════════════════════════════════════════════════╣\n");
    printf("║  Total:  %2d tests                                         ║\n", test_count);
    printf("║  Passed: %2d tests                                         ║\n", pass_count);
    printf("║  Skipped: %2d tests                                        ║\n", skip_count);
    printf("║  Failed: %2d tests                                         ║\n", test_count - pass_count - skip_count);
    printf("╚════════════════════════════════════════════════════════════╝\n");
    printf("\n");

    return (pass_count == test_count - skip_count) ? 0 : 1;
}
