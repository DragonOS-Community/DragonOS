#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

/**
 * Test that link(2) on procfs entries returns EPERM.
 *
 * procfs is a virtual filesystem that does not support hard links.
 * Attempting to create a hard link targeting a procfs entry should
 * fail with EPERM, matching Linux kernel behavior.
 */

#define TEST_PROCFS_PATH "/proc/self/status"
#define TEST_LINK_PATH "/tmp/procfs_link_test"

int main(void) {
    int ret;

    /* Attempt to create a hard link from a procfs entry */
    ret = link(TEST_PROCFS_PATH, TEST_LINK_PATH);

    if (ret == 0) {
        fprintf(stderr,
                "FAIL: link(\"%s\", \"%s\") succeeded unexpectedly\n",
                TEST_PROCFS_PATH, TEST_LINK_PATH);
        unlink(TEST_LINK_PATH);
        return EXIT_FAILURE;
    }

    if (errno != EPERM && errno != EXDEV) {
        fprintf(stderr,
                "FAIL: link() returned errno %d (%m), expected EPERM (%d) "
                "or EXDEV (%d)\n",
                errno, EPERM, EXDEV);
        return EXIT_FAILURE;
    }

    printf("PASS: link() on procfs correctly returned errno %d (%m)\n", errno);
    return EXIT_SUCCESS;
}
