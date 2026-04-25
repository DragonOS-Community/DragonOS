#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static const char *kRangeFile = "/proc/sys/net/ipv4/ip_local_port_range";

static int read_range(int *min, int *max) {
    int fd = open(kRangeFile, O_RDONLY);
    if (fd < 0) {
        perror("open range file");
        return -1;
    }
    char buf[64];
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (n <= 0) {
        perror("read range file");
        return -1;
    }
    buf[n] = '\0';
    if (sscanf(buf, "%d %d", min, max) != 2) {
        fprintf(stderr, "failed to parse range: '%s'\n", buf);
        return -1;
    }
    return 0;
}

static int write_range(int min, int max) {
    int fd = open(kRangeFile, O_WRONLY | O_TRUNC);
    if (fd < 0) {
        perror("open range file for write");
        return -1;
    }
    char buf[64];
    int len = snprintf(buf, sizeof(buf), "%d %d", min, max);
    if (len <= 0) {
        close(fd);
        return -1;
    }
    ssize_t n = write(fd, buf, (size_t)len);
    close(fd);
    if (n != len) {
        perror("write range file");
        return -1;
    }
    return 0;
}

int main(void) {
    int parent_min = 0, parent_max = 0;
    if (read_range(&parent_min, &parent_max) != 0) {
        return 1;
    }

    if (access(kRangeFile, W_OK) != 0) {
        printf("[SKIP] %s not writable\n", kRangeFile);
        return 0;
    }

    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        return 1;
    }
    if (pid == 0) {
        if (unshare(CLONE_NEWNET) != 0) {
            printf("[SKIP] unshare(CLONE_NEWNET) failed: %s\n", strerror(errno));
            return 0;
        }
        int child_min = 0, child_max = 0;
        if (read_range(&child_min, &child_max) != 0) {
            return 1;
        }
        int new_min = child_min;
        int new_max = child_min + 10;
        if (write_range(new_min, new_max) != 0) {
            return 1;
        }
        int verify_min = 0, verify_max = 0;
        if (read_range(&verify_min, &verify_max) != 0) {
            return 1;
        }
        if (verify_min != new_min || verify_max != new_max) {
            fprintf(stderr, "child range verify failed: %d %d (expected %d %d)\n",
                    verify_min, verify_max, new_min, new_max);
            return 1;
        }
        // Restore original range in this netns to keep it tidy.
        if (write_range(child_min, child_max) != 0) {
            return 1;
        }
        return 0;
    }

    int status = 0;
    if (waitpid(pid, &status, 0) < 0) {
        perror("waitpid");
        return 1;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        fprintf(stderr, "child failed\n");
        return 1;
    }

    int parent_min_after = 0, parent_max_after = 0;
    if (read_range(&parent_min_after, &parent_max_after) != 0) {
        return 1;
    }

    if (parent_min_after != parent_min || parent_max_after != parent_max) {
        fprintf(stderr,
                "parent range changed: %d %d (expected %d %d)\n",
                parent_min_after, parent_max_after, parent_min, parent_max);
        return 1;
    }

    printf("[PASS] ip_local_port_range is isolated per netns\n");
    return 0;
}
