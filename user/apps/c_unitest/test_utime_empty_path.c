#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>
#include <utime.h>

/* Use raw syscalls: libc may translate the legacy interfaces to utimensat. */
static unsigned passed, failed;
static const struct timespec initial[2] = {{946684800, 0}, {946684800, 0}};
static const struct timespec changed[2] = {{1000000000, 0}, {1000000001, 0}};
static const struct timespec omitted[2] = {{0, UTIME_OMIT}, {0, UTIME_OMIT}};
static struct stat before_cwd, before_file;

static void fatal(const char *what)
{
    perror(what);
    exit(2);
}

static int same_time(struct timespec a, struct timespec b)
{
    return a.tv_sec == b.tv_sec && a.tv_nsec == b.tv_nsec;
}

static int same_timestamps(const struct stat *a, const struct stat *b)
{
    return same_time(a->st_atim, b->st_atim) && same_time(a->st_mtim, b->st_mtim) && same_time(a->st_ctim, b->st_ctim);
}

static void prepare(void)
{
    if (utimensat(AT_FDCWD, ".", initial, 0) || utimensat(AT_FDCWD, "file", initial, 0) || stat(".", &before_cwd) ||
        stat("file", &before_file))
        fatal("prepare");
    errno = 0;
}

static void check(const char *name, long rc, int saved_errno, int expected_errno, int changed_target)
{
    struct stat cwd, file;
    if (stat(".", &cwd) || stat("file", &file))
        fatal("stat after call");
    int ok = expected_errno ? rc == -1 && saved_errno == expected_errno : rc == 0;
    const struct stat *target = changed_target == 1 ? &cwd : &file;
    if (changed_target)
    {
        ok &= same_time(target->st_atim, changed[0]) && same_time(target->st_mtim, changed[1]);
        ok &= changed_target == 1 ? same_timestamps(&file, &before_file) : same_timestamps(&cwd, &before_cwd);
    }
    else
    {
        ok &= same_timestamps(&cwd, &before_cwd) && same_timestamps(&file, &before_file);
    }
    printf("%s %s rc=%ld errno=%d expected_errno=%d\n", ok ? "PASS" : "FAIL", name, rc, saved_errno, expected_errno);
    if (ok)
        passed++;
    else
        failed++;
}

#define RUN(name, call, expected, target)                                                                              \
    do                                                                                                                 \
    {                                                                                                                  \
        prepare();                                                                                                     \
        long result = (call);                                                                                          \
        int saved = errno;                                                                                             \
        check(name, result, saved, expected, target);                                                                  \
    } while (0)

int main(int argc, char **argv)
{
    char directory[512];
    const char *parent = argc > 1 ? argv[1] : "/tmp";
    if (snprintf(directory, sizeof(directory), "%s/utime-empty.XXXXXX", parent) >= (int)sizeof(directory))
        return 2;
    if (!mkdtemp(directory) || chdir(directory))
        fatal("mkdtemp/chdir");
    int fd = open("file", O_CREAT | O_RDWR, 0600);
    int dirfd = open(".", O_RDONLY | O_DIRECTORY);
    if (fd < 0 || dirfd < 0 || symlink("file", "link"))
        fatal("fixture");

    RUN("utimensat empty/current-time", syscall(SYS_utimensat, AT_FDCWD, "", NULL, 0), ENOENT, 0);
    RUN("utimensat empty/explicit-time", syscall(SYS_utimensat, AT_FDCWD, "", changed, 0), ENOENT, 0);
    RUN("utimensat empty/directory-fd", syscall(SYS_utimensat, dirfd, "", changed, 0), ENOENT, 0);
    RUN("utimensat empty/regular-fd", syscall(SYS_utimensat, fd, "", changed, 0), ENOENT, 0);
    RUN("utimensat empty/invalid-fd", syscall(SYS_utimensat, -1, "", NULL, 0), ENOENT, 0);
    RUN("utimensat empty/nofollow", syscall(SYS_utimensat, AT_FDCWD, "", NULL, AT_SYMLINK_NOFOLLOW), ENOENT, 0);
    RUN("utimensat empty/both-omit", syscall(SYS_utimensat, AT_FDCWD, "", omitted, 0), 0, 0);
    RUN("utimensat bad-fd/both-omit", syscall(SYS_utimensat, -1, "", omitted, 0), 0, 0);
    RUN("utimensat missing", syscall(SYS_utimensat, AT_FDCWD, "absent", NULL, 0), ENOENT, 0);
    RUN("utimensat dot", syscall(SYS_utimensat, AT_FDCWD, ".", changed, 0), 0, 1);
    RUN("utimensat named-file", syscall(SYS_utimensat, dirfd, "file", changed, 0), 0, 2);
    RUN("utimensat follow-symlink", syscall(SYS_utimensat, AT_FDCWD, "link", changed, 0), 0, 2);
    RUN("utimensat NULL/futimens", syscall(SYS_utimensat, fd, NULL, changed, 0), 0, 2);
    RUN("utimensat NULL/AT_FDCWD", syscall(SYS_utimensat, AT_FDCWD, NULL, changed, 0), EFAULT, 0);
#ifdef SYS_utimes
    struct timeval values[2] = {{1000000000, 0}, {1000000001, 0}};
    struct timeval invalid[2] = {{0, 1000000}, {0, 0}};
    RUN("utimes empty/current-time", syscall(SYS_utimes, "", NULL), ENOENT, 0);
    RUN("utimes empty/explicit-time", syscall(SYS_utimes, "", values), ENOENT, 0);
    RUN("utimes invalid-usec/empty", syscall(SYS_utimes, "", invalid), EINVAL, 0);
    RUN("utimes named-file", syscall(SYS_utimes, "file", values), 0, 2);
#endif
#ifdef SYS_futimesat
    RUN("futimesat empty", syscall(SYS_futimesat, AT_FDCWD, "", NULL), ENOENT, 0);
    RUN("futimesat empty/directory-fd", syscall(SYS_futimesat, dirfd, "", NULL), ENOENT, 0);
#endif
#ifdef SYS_utime
    struct utimbuf legacy = {1000000000, 1000000001};
    RUN("utime empty/current-time", syscall(SYS_utime, "", NULL), ENOENT, 0);
    RUN("utime empty/explicit-time", syscall(SYS_utime, "", &legacy), ENOENT, 0);
    RUN("utime named-file", syscall(SYS_utime, "file", &legacy), 0, 2);
#endif
    if (close(fd) || close(dirfd) || unlink("link") || unlink("file") || chdir("/") || rmdir(directory))
        fatal("cleanup");
    printf("RESULT passed=%u failed=%u\n", passed, failed);
    return failed ? 1 : 0;
}
