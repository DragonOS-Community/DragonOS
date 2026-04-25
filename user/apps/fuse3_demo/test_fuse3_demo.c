#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static void log_fail(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
}

static int read_all(const char *path, char *buf, size_t cap) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return -1;
    }
    ssize_t n = read(fd, buf, cap - 1);
    int saved = errno;
    close(fd);
    if (n < 0) {
        errno = saved;
        return -1;
    }
    buf[n] = '\0';
    return (int)n;
}

static int wait_hello_ready(const char *mountpoint, int timeout_ms) {
    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mountpoint);

    int rounds = timeout_ms / 20;
    if (rounds < 1) {
        rounds = 1;
    }

    for (int i = 0; i < rounds; i++) {
        char buf[128];
        if (read_all(path, buf, sizeof(buf)) >= 0) {
            if (strncmp(buf, "hello from libfuse3\n", 20) == 0) {
                return 0;
            }
        }
        usleep(20 * 1000);
    }
    errno = ETIMEDOUT;
    return -1;
}

static int stop_daemon(pid_t pid, int *status) {
    for (int i = 0; i < 100; i++) {
        pid_t w = waitpid(pid, status, WNOHANG);
        if (w == pid) {
            return 0;
        }
        usleep(20 * 1000);
    }

    kill(pid, SIGINT);
    for (int i = 0; i < 100; i++) {
        pid_t w = waitpid(pid, status, WNOHANG);
        if (w == pid) {
            return 0;
        }
        usleep(20 * 1000);
    }

    kill(pid, SIGTERM);
    for (int i = 0; i < 100; i++) {
        pid_t w = waitpid(pid, status, WNOHANG);
        if (w == pid) {
            return 0;
        }
        usleep(20 * 1000);
    }

    kill(pid, SIGKILL);
    return waitpid(pid, status, 0) == pid ? 0 : -1;
}

int main(void) {
    char mnt_template[] = "/tmp/test_fuse3_demo_XXXXXX";
    char *mountpoint = mkdtemp(mnt_template);
    if (!mountpoint) {
        log_fail("[FAIL] mkdtemp mountpoint: %s (errno=%d)\n", strerror(errno), errno);
        return 1;
    }

    const char *daemon_path = "/bin/fuse3_demo";
    if (access(daemon_path, X_OK) != 0) {
        daemon_path = "./fuse3_demo";
    }

    pid_t pid = fork();
    if (pid < 0) {
        log_fail("[FAIL] fork: %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mountpoint);
        return 1;
    }

    if (pid == 0) {
        execl(daemon_path, daemon_path, mountpoint, "--single", NULL);
        _exit(127);
    }

    if (wait_hello_ready(mountpoint, 5000) != 0) {
        log_fail("[FAIL] wait hello ready: %s (errno=%d)\n", strerror(errno), errno);
        int st = 0;
        stop_daemon(pid, &st);
        umount(mountpoint);
        rmdir(mountpoint);
        return 1;
    }

    char note[256];
    snprintf(note, sizeof(note), "%s/note.txt", mountpoint);
    int fd = open(note, O_CREAT | O_RDWR | O_TRUNC, 0644);
    if (fd < 0) {
        log_fail("[FAIL] create note: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    const char *content = "dragonos fuse3 test\n";
    ssize_t wn = write(fd, content, strlen(content));
    close(fd);
    if (wn != (ssize_t)strlen(content)) {
        log_fail("[FAIL] write note: wn=%zd errno=%d (%s)\n", wn, errno, strerror(errno));
        goto fail;
    }

    char read_buf[256];
    if (read_all(note, read_buf, sizeof(read_buf)) < 0) {
        log_fail("[FAIL] read note: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (strcmp(read_buf, content) != 0) {
        log_fail("[FAIL] content mismatch: got='%s' expect='%s'\n", read_buf, content);
        goto fail;
    }

    char renamed[256];
    snprintf(renamed, sizeof(renamed), "%s/note2.txt", mountpoint);
    if (rename(note, renamed) != 0) {
        log_fail("[FAIL] rename note: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    if (unlink(renamed) != 0) {
        log_fail("[FAIL] unlink note2: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    int dirfd = open(mountpoint, O_RDONLY | O_DIRECTORY);
    if (dirfd < 0) {
        log_fail("[FAIL] open mountpoint dir: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (fsync(dirfd) != 0) {
        log_fail("[FAIL] fsyncdir mountpoint: %s (errno=%d)\n", strerror(errno), errno);
        close(dirfd);
        goto fail;
    }
    close(dirfd);

    if (umount(mountpoint) != 0) {
        log_fail("[FAIL] umount(%s): %s (errno=%d)\n", mountpoint, strerror(errno), errno);
        int status = 0;
        stop_daemon(pid, &status);
        rmdir(mountpoint);
        return 1;
    }

    {
        int status = 0;
        if (stop_daemon(pid, &status) != 0) {
            log_fail("[FAIL] stop daemon failed\n");
            rmdir(mountpoint);
            return 1;
        }
        if (!WIFEXITED(status)) {
            log_fail("[FAIL] daemon not exited normally, status=%d\n", status);
            rmdir(mountpoint);
            return 1;
        }
        int code = WEXITSTATUS(status);
        if (code != 0 && code != 8) {
            log_fail("[FAIL] daemon exit code=%d (raw=%d)\n", code, status);
            rmdir(mountpoint);
            return 1;
        }
    }

    rmdir(mountpoint);
    printf("[PASS] fuse3_demo\n");
    return 0;

fail:
    {
        int status = 0;
        stop_daemon(pid, &status);
    }
    umount(mountpoint);
    rmdir(mountpoint);
    return 1;
}
