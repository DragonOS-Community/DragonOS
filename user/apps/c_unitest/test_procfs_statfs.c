#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <string.h>
#include <sys/statfs.h>
#include <unistd.h>

#define PROC_SUPER_MAGIC 0x9fa0L

static int check_field(const char *path, const char *operation,
                       const char *field, long long actual,
                       long long expected) {
    if (actual == expected) {
        return 0;
    }
    fprintf(stderr, "%s(%s): %s=%lld, expected %lld\n",
            operation, path, field, actual, expected);
    return 1;
}

static int check_metadata(const char *path, const char *operation,
                          const struct statfs *info, long page_size) {
    int failures = 0;

    failures += check_field(path, operation, "f_type", info->f_type,
                            PROC_SUPER_MAGIC);
    failures += check_field(path, operation, "f_bsize", info->f_bsize,
                            page_size);
    failures += check_field(path, operation, "f_frsize", info->f_frsize,
                            page_size);
    failures += check_field(path, operation, "f_namelen", info->f_namelen,
                            NAME_MAX);
    printf("%s(%s): type=%llx bsize=%lld frsize=%lld namelen=%lld\n",
           operation, path, (unsigned long long)info->f_type,
           (long long)info->f_bsize, (long long)info->f_frsize,
           (long long)info->f_namelen);
    return failures;
}

int main(void) {
    const char *paths[] = {"/proc", "/proc/self", "/proc/uptime"};
    const long page_size = sysconf(_SC_PAGESIZE);
    int failures = 0;

    if (page_size <= 0) {
        fprintf(stderr, "sysconf(_SC_PAGESIZE) failed\n");
        return 1;
    }

    for (size_t i = 0; i < sizeof(paths) / sizeof(paths[0]); ++i) {
        struct statfs info;
        int fd;

        if (statfs(paths[i], &info) != 0) {
            fprintf(stderr, "statfs(%s): %s\n", paths[i], strerror(errno));
            ++failures;
        } else {
            failures += check_metadata(paths[i], "statfs", &info, page_size);
        }

        fd = open(paths[i], O_RDONLY | O_CLOEXEC);
        if (fd < 0) {
            fprintf(stderr, "open(%s): %s\n", paths[i], strerror(errno));
            ++failures;
            continue;
        }
        if (fstatfs(fd, &info) != 0) {
            fprintf(stderr, "fstatfs(%s): %s\n", paths[i], strerror(errno));
            ++failures;
        } else {
            failures += check_metadata(paths[i], "fstatfs", &info, page_size);
        }
        if (close(fd) != 0) {
            fprintf(stderr, "close(%s): %s\n", paths[i], strerror(errno));
            ++failures;
        }
    }

    printf("procfs statfs metadata: %s (%d failures)\n",
           failures == 0 ? "PASS" : "FAIL", failures);
    return failures == 0 ? 0 : 1;
}
