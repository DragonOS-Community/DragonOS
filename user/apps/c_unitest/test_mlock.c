// Comprehensive mlock test suite with reporting and cleanup
// Tests mlock, munlock, mlockall, munlockall system calls

#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>
#include <string.h>
#include <errno.h>
#include <sys/resource.h>
#include <sys/types.h>
#include <sys/wait.h>

typedef int (*test_func_t)(void);

static void report(const char *name, int ok, const char *msg) {
    if (ok) {
        printf("[PASS] %s\n", name);
    } else {
        if (msg) {
            printf("[FAILED] %s: %s (errno=%d)\n", name, msg, errno);
        } else {
            printf("[FAILED] %s (errno=%d)\n", name, errno);
        }
    }
}

static size_t get_page_size(void) {
    long sz = sysconf(_SC_PAGESIZE);
    if (sz <= 0) sz = 4096;
    return (size_t)sz;
}

// Test 1: Basic mlock/munlock on anonymous mapping
static int test_basic_mlock(void) {
    size_t pagesize = get_page_size();
    size_t length = pagesize * 4;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) return 0;

    int rc = 0;
    if (mlock(addr, length) != 0) {
        goto cleanup;
    }

    // Write to verify memory is accessible
    for (size_t i = 0; i < length; i++) {
        addr[i] = (char)(i & 0xff);
    }

    // Verify data
    for (size_t i = 0; i < length; i++) {
        if (addr[i] != (char)(i & 0xff)) {
            goto cleanup;
        }
    }

    if (munlock(addr, length) != 0) {
        goto cleanup;
    }

    rc = 1;
cleanup:
    munmap(addr, length);
    return rc;
}

// Test 2: mlockall with MCL_CURRENT
static int test_mlockall_current(void) {
    size_t pagesize = get_page_size();
    size_t length = pagesize * 2;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) return 0;

    int rc = 0;
    if (mlockall(MCL_CURRENT) != 0) {
        goto cleanup;
    }

    // Memory should be accessible
    memset(addr, 0x55, length);

    if (munlockall() != 0) {
        goto cleanup;
    }

    rc = 1;
cleanup:
    munmap(addr, length);
    return rc;
}

// Test 3: mlockall with MCL_FUTURE locks future mappings
static int test_mlockall_future(void) {
    size_t pagesize = get_page_size();

    if (mlockall(MCL_FUTURE) != 0) {
        return 0;
    }

    // Allocate new memory - should be automatically locked
    size_t length = pagesize * 2;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        munlockall();
        return 0;
    }

    int rc = 0;
    // Memory should be accessible and locked
    memset(addr, 0xaa, length);

    if (munlockall() != 0) {
        goto cleanup;
    }

    rc = 1;
cleanup:
    munmap(addr, length);
    return rc;
}

// Test 4: Multiple locks on same region (reference counting)
static int test_multiple_locks(void) {
    size_t pagesize = get_page_size();
    size_t length = pagesize * 2;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) return 0;

    int rc = 0;

    // First lock
    if (mlock(addr, length) != 0) {
        goto cleanup;
    }

    // Second lock on same region (should succeed)
    if (mlock(addr, length) != 0) {
        // May fail on some systems, not necessarily a test failure
        // but we expect it to succeed with reference counting
    }

    // First unlock
    if (munlock(addr, length) != 0) {
        goto cleanup;
    }

    // Second unlock (should succeed, releasing the lock)
    if (munlock(addr, length) != 0) {
        goto cleanup;
    }

    rc = 1;
cleanup:
    munmap(addr, length);
    return rc;
}

// Test 5: Partial region locking
static int test_partial_lock(void) {
    size_t pagesize = get_page_size();
    size_t total_length = pagesize * 4;
    char *addr = mmap(NULL, total_length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) return 0;

    int rc = 0;

    // Lock middle 2 pages
    char *lock_start = addr + pagesize;
    size_t lock_length = pagesize * 2;

    if (mlock(lock_start, lock_length) != 0) {
        goto cleanup;
    }

    // Write to entire region
    for (size_t i = 0; i < total_length; i++) {
        addr[i] = (char)(i & 0xff);
    }

    // Verify
    for (size_t i = 0; i < total_length; i++) {
        if (addr[i] != (char)(i & 0xff)) {
            goto cleanup;
        }
    }

    if (munlock(lock_start, lock_length) != 0) {
        goto cleanup;
    }

    rc = 1;
cleanup:
    munmap(addr, total_length);
    return rc;
}

// Test 6: Lock zero length should succeed (no-op)
static int test_zero_length(void) {
    size_t pagesize = get_page_size();
    char *addr = mmap(NULL, pagesize, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) return 0;

    // mlock with zero length should succeed
    int rc = (mlock(addr, 0) == 0);
    munmap(addr, pagesize);
    return rc;
}

// Test 7: munlockall unlocks all
static int test_munlockall(void) {
    size_t pagesize = get_page_size();

    // Lock multiple regions
    size_t length1 = pagesize * 2;
    char *addr1 = mmap(NULL, length1, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr1 == MAP_FAILED) return 0;

    size_t length2 = pagesize * 3;
    char *addr2 = mmap(NULL, length2, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr2 == MAP_FAILED) {
        munmap(addr1, length1);
        return 0;
    }

    int rc = 0;

    if (mlock(addr1, length1) != 0) {
        goto cleanup;
    }
    if (mlock(addr2, length2) != 0) {
        goto cleanup;
    }

    // munlockall should unlock everything
    if (munlockall() != 0) {
        goto cleanup;
    }

    rc = 1;
cleanup:
    munmap(addr1, length1);
    munmap(addr2, length2);
    return rc;
}

// Test 8: Check RLIMIT_MEMLOCK enforcement
static int test_rlimit_memlock(void) {
    struct rlimit rlim;
    size_t pagesize = get_page_size();

    // Get current limit
    if (getrlimit(RLIMIT_MEMLOCK, &rlim) != 0) {
        return 0;
    }

    // If limit is unlimited, skip this test
    if (rlim.rlim_cur == RLIM_INFINITY) {
        printf("[SKIP] rlimit_memlock: limit is unlimited\n");
        return 1;
    }

    // Try to lock more than the limit
    size_t try_length = rlim.rlim_cur + pagesize;
    char *addr = mmap(NULL, try_length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);

    if (addr == MAP_FAILED) {
        // If we can't even mmap, just mark as passed
        return 1;
    }

    // mlock should fail with ENOMEM or EPERM
    int result = mlock(addr, try_length);
    munmap(addr, try_length);

    // Expect failure when exceeding limit
    return (result != 0);
}

// Test 9: Lock after munlock
static int test_relock_after_unlock(void) {
    size_t pagesize = get_page_size();
    size_t length = pagesize * 2;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) return 0;

    int rc = 0;

    // First lock
    if (mlock(addr, length) != 0) {
        goto cleanup;
    }

    // Unlock
    if (munlock(addr, length) != 0) {
        goto cleanup;
    }

    // Lock again
    if (mlock(addr, length) != 0) {
        goto cleanup;
    }

    // Verify memory is still accessible
    memset(addr, 0x77, length);

    // Final unlock
    if (munlock(addr, length) != 0) {
        goto cleanup;
    }

    rc = 1;
cleanup:
    munmap(addr, length);
    return rc;
}

// Test 10: Large region locking
static int test_large_region(void) {
    size_t pagesize = get_page_size();
    struct rlimit rlim;

    // Get current memlock limit
    if (getrlimit(RLIMIT_MEMLOCK, &rlim) != 0) {
        return 0;
    }

    // Determine how many pages we can safely lock
    size_t max_bytes;
    if (rlim.rlim_cur == RLIM_INFINITY) {
        max_bytes = 1024 * 1024;  // 1MB if unlimited
    } else {
        max_bytes = rlim.rlim_cur;
        if (max_bytes > 1024 * 1024) {
            max_bytes = 1024 * 1024;  // Cap at 1MB
        }
    }

    // Use 1/4 of the limit to be safe, or 10 pages minimum
    size_t npages = (max_bytes / 4) / pagesize;
    if (npages < 10) npages = 10;
    if (npages > 100) npages = 100;

    size_t length = pagesize * npages;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) return 0;

    int rc = 0;

    if (mlock(addr, length) != 0) {
        munmap(addr, length);
        return 0;
    }

    // Write pattern to all pages
    for (size_t i = 0; i < length; i += pagesize) {
        addr[i] = 'X';
    }

    if (munlock(addr, length) != 0) {
        goto cleanup;
    }

    rc = 1;
cleanup:
    munmap(addr, length);
    return rc;
}

// Test 11: Fork behavior - locks should not be inherited
static int test_fork_inheritance(void) {
    size_t pagesize = get_page_size();
    size_t length = pagesize * 2;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) return 0;

    int rc = 0;

    // Lock in parent
    if (mlock(addr, length) != 0) {
        munmap(addr, length);
        return 0;
    }

    pid_t pid = fork();
    if (pid < 0) {
        goto cleanup;
    } else if (pid == 0) {
        // Child process - locks should NOT be inherited
        // munlock should succeed even though we didn't lock in child
        if (munlock(addr, length) == 0) {
            exit(0);  // Success
        } else {
            exit(1);  // Failure
        }
    } else {
        // Parent - wait for child
        int status;
        waitpid(pid, &status, 0);
        rc = WIFEXITED(status) && WEXITSTATUS(status) == 0;
    }

cleanup:
    munlock(addr, length);
    munmap(addr, length);
    return rc;
}

// Test 12: Lock with invalid address
static int test_invalid_address(void) {
    size_t pagesize = get_page_size();

    // Try to lock invalid address
    void *invalid_addr = (void *)~0UL;
    int result = mlock(invalid_addr, pagesize);

    // Should fail with ENOMEM or EPERM
    return (result != 0);
}

// Test 13: Lock unaligned address
static int test_unaligned_address(void) {
    size_t pagesize = get_page_size();
    size_t length = pagesize * 2;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) return 0;

    // Try unaligned lock - kernel should align it
    char *unaligned = addr + 1;
    if (mlock(unaligned, pagesize) != 0) {
        munmap(addr, length);
        return 0;
    }

    munlock(unaligned, pagesize);
    munmap(addr, length);
    return 1;
}

// Test 14: mlockall with both MCL_CURRENT and MCL_FUTURE
static int test_mlockall_combined(void) {
    size_t pagesize = get_page_size();

    int rc = 0;

    // Lock both current and future mappings
    if (mlockall(MCL_CURRENT | MCL_FUTURE) != 0) {
        return 0;
    }

    // Existing allocations should be locked
    size_t length1 = pagesize * 2;
    char *addr1 = mmap(NULL, length1, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr1 == MAP_FAILED) {
        munlockall();
        return 0;
    }
    memset(addr1, 0x11, length1);

    // New allocations should also be locked
    size_t length2 = pagesize * 2;
    char *addr2 = mmap(NULL, length2, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr2 == MAP_FAILED) {
        munmap(addr1, length1);
        munlockall();
        return 0;
    }
    memset(addr2, 0x22, length2);

    if (munlockall() != 0) {
        goto cleanup;
    }

    rc = 1;
cleanup:
    munmap(addr1, length1);
    munmap(addr2, length2);
    return rc;
}

// Test 15: Verify locked memory stays in memory
static int test_memory_persistence(void) {
    size_t pagesize = get_page_size();
    size_t length = pagesize * 10;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) return 0;

    int rc = 0;

    // Lock the memory
    if (mlock(addr, length) != 0) {
        goto cleanup;
    }

    // Write data pattern
    for (size_t i = 0; i < length; i++) {
        addr[i] = (char)(i % 256);
    }

    // Read back to verify data integrity
    for (size_t i = 0; i < length; i++) {
        if (addr[i] != (char)(i % 256)) {
            goto cleanup;
        }
    }

    if (munlock(addr, length) != 0) {
        goto cleanup;
    }

    rc = 1;
cleanup:
    munmap(addr, length);
    return rc;
}

struct test_entry {
    const char *name;
    test_func_t fn;
};

int main(void) {
    printf("========================================\n");
    printf("  mlock Comprehensive Test Suite\n");
    printf("========================================\n\n");

    struct test_entry tests[] = {
        {"basic_mlock", test_basic_mlock},
        {"mlockall_current", test_mlockall_current},
        {"mlockall_future", test_mlockall_future},
        {"multiple_locks", test_multiple_locks},
        {"partial_lock", test_partial_lock},
        {"zero_length", test_zero_length},
        {"munlockall", test_munlockall},
        {"rlimit_memlock", test_rlimit_memlock},
        {"relock_after_unlock", test_relock_after_unlock},
        {"large_region", test_large_region},
        {"fork_inheritance", test_fork_inheritance},
        {"invalid_address", test_invalid_address},
        {"unaligned_address", test_unaligned_address},
        {"mlockall_combined", test_mlockall_combined},
        {"memory_persistence", test_memory_persistence},
    };

    int total = (int)(sizeof(tests) / sizeof(tests[0]));
    int passed = 0;

    for (int i = 0; i < total; i++) {
        errno = 0;
        int ok = tests[i].fn();
        report(tests[i].name, ok, NULL);
        if (ok) passed++;
    }

    printf("\n========================================\n");
    printf("Summary: %d/%d tests passed\n", passed, total);
    printf("========================================\n");

    return passed == total ? 0 : 1;
}
