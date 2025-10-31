// Unified mincore test suite with reporting and cleanup

#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>

typedef int (*test_func_t)(void);

static void report(const char *name, int ok, const char *msg) {
    if (ok) {
        printf("[PASS] %s\n", name);
    } else {
        if (msg) {
            printf("[FAILED] %s: %s\n", name, msg);
        } else {
            printf("[FAILED] %s\n", name);
        }
    }
}

// Test 1: Anonymous mapping pages become resident after write
static int test_anonymous_incore(void) {
    size_t pagesize = (size_t)sysconf(_SC_PAGESIZE);
    size_t npages = 4;
    size_t length = pagesize * npages;
    int rc = 1;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) return 0;
    memset(addr, 0xaa, length);
    unsigned char *vec = (unsigned char *)malloc(npages);
    if (!vec) { munmap(addr, length); return 0; }
    if (mincore(addr, length, vec) == -1) { free(vec); munmap(addr, length); return 0; }
    for (size_t i = 0; i < npages; i++) {
        if (!(vec[i] & 1)) { rc = 0; break; }
    }
    free(vec);
    munmap(addr, length);
    return rc;
}

// Test 2: Unaligned addr -> EINVAL
static int test_unaligned_einval(void) {
    size_t pagesize = (size_t)sysconf(_SC_PAGESIZE);
    char *addr = mmap(NULL, pagesize, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) return 0;
    unsigned char vec[1];
    errno = 0;
    int ret = mincore(addr + 1, pagesize, vec);
    int ok = (ret == -1 && errno == EINVAL);
    munmap(addr, pagesize);
    return ok;
}

// Test 3: len == 0 -> EINVAL
static int test_len0_einval(void) {
    size_t pagesize = (size_t)sysconf(_SC_PAGESIZE);
    char *addr = mmap(NULL, pagesize, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) return 0;
    unsigned char vec[1];
    errno = 0;
    int ret = mincore(addr, 0, vec);
    int ok = (ret == -1 && errno == EINVAL);
    munmap(addr, pagesize);
    return ok;
}

// Test 4: Range crosses a hole -> ENOMEM
static int test_range_hole_enomem(void) {
    size_t pagesize = (size_t)sysconf(_SC_PAGESIZE);
    size_t length = pagesize * 2;
    char *addr = mmap(NULL, length, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) return 0;
    if (munmap(addr + pagesize, pagesize) != 0) { munmap(addr, pagesize); return 0; }
    unsigned char vec[2] = {0};
    errno = 0;
    int ret = mincore(addr, length, vec);
    int ok = (ret == -1 && errno == ENOMEM);
    munmap(addr, pagesize);
    return ok;
}

// Test 5: vec not writable -> EFAULT
static int test_vec_efault(void) {
    size_t pagesize = (size_t)sysconf(_SC_PAGESIZE);
    char *addr = mmap(NULL, pagesize, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) return 0;
    char *ro = mmap(NULL, pagesize, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (ro == MAP_FAILED) { munmap(addr, pagesize); return 0; }
    errno = 0;
    int ret = mincore(addr, pagesize, (unsigned char*)ro);
    int ok = (ret == -1 && errno == EFAULT);
    munmap(addr, pagesize);
    munmap(ro, pagesize);
    return ok;
}

// Test 6: file-backed mapping reflects page cache presence after read
static int test_filemap_pagecache(void) {
    size_t pagesize = (size_t)sysconf(_SC_PAGESIZE);
    char tmpl[] = "mincore_test_file_XXXXXX";
    int fd = mkstemp(tmpl);
    if (fd < 0) return 0;
    // write 2 pages
    char *buf = (char*)malloc(pagesize * 2);
    if (!buf) { close(fd); unlink(tmpl); return 0; }
    memset(buf, 0xab, pagesize * 2);
    ssize_t w = write(fd, buf, pagesize * 2);
    free(buf);
    if (w != (ssize_t)(pagesize * 2)) { close(fd); unlink(tmpl); return 0; }
    char *addr = mmap(NULL, pagesize * 2, PROT_READ, MAP_PRIVATE, fd, 0);
    if (addr == MAP_FAILED) { close(fd); unlink(tmpl); return 0; }
    unsigned char vec_before[2] = {0};
    if (mincore(addr, pagesize * 2, vec_before) != 0) {
        munmap(addr, pagesize * 2); close(fd); unlink(tmpl); return 0;
    }
    volatile char c = addr[0]; (void)c; // fault-in first page
    unsigned char vec_after[2] = {0};
    if (mincore(addr, pagesize * 2, vec_after) != 0) {
        munmap(addr, pagesize * 2); close(fd); unlink(tmpl); return 0;
    }
    int ok = ((vec_after[0] & 1) == 1);
    munmap(addr, pagesize * 2);
    close(fd);
    unlink(tmpl);
    return ok;
}

struct test_entry { const char *name; test_func_t fn; };

int main(void) {
    struct test_entry tests[] = {
        {"anonymous_incore", test_anonymous_incore},
        {"unaligned_einval", test_unaligned_einval},
        {"len0_einval", test_len0_einval},
        {"range_hole_enomem", test_range_hole_enomem},
        {"vec_efault", test_vec_efault},
        {"filemap_pagecache", test_filemap_pagecache},
    };
    int total = (int)(sizeof(tests)/sizeof(tests[0]));
    int passed = 0;
    for (int i = 0; i < total; i++) {
        int ok = tests[i].fn();
        report(tests[i].name, ok, NULL);
        if (ok) passed++;
    }
    printf("Summary: %d/%d passed\n", passed, total);
    return passed == total ? 0 : 1;
}
