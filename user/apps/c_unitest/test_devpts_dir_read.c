#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define DEVPTS_DIR "/dev/pts"

static int g_total = 0;
static int g_failed = 0;

#define CHECK(cond, msg)                                                       \
    do {                                                                       \
        g_total++;                                                             \
        if (!(cond)) {                                                         \
            g_failed++;                                                        \
            fprintf(stderr, "FAIL: %s (line %d)\n", msg, __LINE__);            \
        } else {                                                               \
            printf("PASS: %s\n", msg);                                         \
        }                                                                      \
    } while (0)

int main(void) {
    struct stat st;
    int fd;
    char buf[16];
    ssize_t nread;

    CHECK(stat(DEVPTS_DIR, &st) == 0, "stat /dev/pts succeeds");
    if (stat(DEVPTS_DIR, &st) != 0) {
        perror("stat /dev/pts");
        return 1;
    }

    CHECK(S_ISDIR(st.st_mode), "/dev/pts is a directory");

    fd = open(DEVPTS_DIR, O_RDONLY | O_DIRECTORY);
    CHECK(fd >= 0, "open /dev/pts with O_RDONLY|O_DIRECTORY succeeds");
    if (fd < 0) {
        perror("open /dev/pts");
        return 1;
    }

    errno = 0;
    nread = read(fd, buf, sizeof(buf));
    CHECK(nread == -1, "read on /dev/pts fails");
    CHECK(errno == EISDIR, "read on /dev/pts returns EISDIR");
    if (nread != -1 || errno != EISDIR) {
        fprintf(stderr, "read returned %zd, errno=%d (%s)\n", nread, errno,
                strerror(errno));
    }

    close(fd);

    if (g_failed == 0) {
        printf("All %d checks passed.\n", g_total);
        return 0;
    }

    fprintf(stderr, "%d/%d checks failed.\n", g_failed, g_total);
    return 1;
}
