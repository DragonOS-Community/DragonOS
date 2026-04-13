/**
 * @file test_fchown_o_path.c
 * @brief Regression test for fchown with O_PATH file descriptor
 *
 * Acceptance Criteria:
 * 1. fchown(o_path_fd, uid, gid) returns -1 with errno == EBADF
 * 2. fchown on normally-opened fd continues to work correctly
 *    (or returns EPERM if lacking CAP_CHOWN, but not EBADF)
 *
 * Background:
 * O_PATH file descriptors are only intended for file operations
 * that operate on the pathname itself, not the file contents.
 * fchown(2) should fail with EBADF on O_PATH fds.
 *
 * Note: musl libc implements fchown() via fchownat() syscall.
 * This test directly invokes SYS_fchown to test the raw syscall.
 */

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <errno.h>
#include <string.h>
#include <sys/stat.h>
#include <syscall.h>

#define TEST_FILE "/tmp/test_fchown_o_path_file"

void cleanup() {
    unlink(TEST_FILE);
}

int main() {
    int o_path_fd = -1;
    int normal_fd = -1;
    int ret;
    int test_passed = 1;
    uid_t test_uid = 0;
    gid_t test_gid = 0;

    // Create a test file
    normal_fd = open(TEST_FILE, O_CREAT | O_RDWR, 0644);
    if (normal_fd < 0) {
        perror("open: failed to create test file");
        return 1;
    }
    close(normal_fd);

    printf("=== fchown O_PATH Regression Test ===\n");
    printf("Testing raw SYS_fchown syscall (bypassing libc wrapper)\n\n");

    // Test 1: Raw fchown syscall on O_PATH fd should return EBADF
    printf("[Test 1] syscall(SYS_fchown, O_PATH fd)\n");
    o_path_fd = open(TEST_FILE, O_PATH);
    if (o_path_fd < 0) {
        perror("open: O_PATH failed");
        cleanup();
        return 1;
    }

    errno = 0;
    ret = syscall(SYS_fchown, o_path_fd, test_uid, test_gid);

    if (ret == -1 && errno == EBADF) {
        printf("  ✓ PASS: SYS_fchown(O_PATH fd) returned -1 with errno=EBADF\n");
    } else {
        printf("  ✗ FAIL: SYS_fchown(O_PATH fd) returned ret=%d, errno=%d (expected EBADF=%d)\n",
               ret, errno, EBADF);
        test_passed = 0;
    }
    close(o_path_fd);

    // Test 2: Raw fchown syscall on normal fd should not return EBADF
    printf("\n[Test 2] syscall(SYS_fchown, normal fd)\n");
    normal_fd = open(TEST_FILE, O_RDONLY);
    if (normal_fd < 0) {
        perror("open: O_RDONLY failed");
        cleanup();
        return 1;
    }

    errno = 0;
    ret = syscall(SYS_fchown, normal_fd, test_uid, test_gid);

    if (ret == -1) {
        if (errno == EBADF) {
            printf("  ✗ FAIL: SYS_fchown(normal fd) returned EBADF (unexpected)\n");
            test_passed = 0;
        } else if (errno == EPERM || errno == EACCES) {
            printf("  ✓ PASS: SYS_fchown(normal fd) failed with %s (expected - permission denied)\n",
                   strerror(errno));
        } else {
            printf("  ? INFO: SYS_fchown(normal fd) returned %s\n", strerror(errno));
        }
    } else {
        printf("  ✓ PASS: SYS_fchown(normal fd) succeeded\n");
    }
    close(normal_fd);

    // Test 3: libc wrapper fchown() for comparison (may use fchownat)
    printf("\n[Test 3] libc wrapper fchown() - for comparison\n");
    o_path_fd = open(TEST_FILE, O_PATH);
    if (o_path_fd < 0) {
        perror("open: O_PATH failed");
        cleanup();
        return 1;
    }

    errno = 0;
    ret = fchown(o_path_fd, test_uid, test_gid);

    if (ret == -1 && errno == EBADF) {
        printf("  ✓ PASS: fchown(O_PATH fd) returned -1 with errno=EBADF\n");
    } else if (ret == -1 && errno == ENOTSUP) {
        printf("  ⚠ INFO: fchown(O_PATH fd) returned ENOTSUP (libc may use fchownat)\n");
    } else {
        printf("  ? INFO: fchown(O_PATH fd) returned ret=%d, errno=%d\n", ret, errno);
    }
    close(o_path_fd);

    // Cleanup
    cleanup();

    // Summary
    printf("\n=== Test Summary ===\n");
    if (test_passed) {
        printf("✓ ALL TESTS PASSED\n");
        return 0;
    } else {
        printf("✗ SOME TESTS FAILED\n");
        return 1;
    }
}
