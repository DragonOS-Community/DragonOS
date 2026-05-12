#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static const char *PARENT = "/sys/fs/cgroup/exec_dethread_parent";
static const char *CHILD = "/sys/fs/cgroup/exec_dethread_parent/child";
static const char *CHILD_PROCS = "/sys/fs/cgroup/exec_dethread_parent/child/cgroup.procs";
static const char *PARENT_CURRENT = "/sys/fs/cgroup/exec_dethread_parent/pids.current";
static const char *CHILD_CURRENT = "/sys/fs/cgroup/exec_dethread_parent/child/pids.current";
static const char *PARENT_MAX = "/sys/fs/cgroup/exec_dethread_parent/pids.max";

static atomic_int exec_ready;

static void fail(const char *step) {
    printf("[FAIL] %s: %s\n", step, strerror(errno));
    exit(1);
}

static int ensure_dir(const char *path) {
    if (mkdir(path, 0755) == 0) {
        return 0;
    }
    if (errno == EEXIST) {
        return 0;
    }
    return -1;
}

static int write_text(const char *path, const char *text) {
    int fd = open(path, O_WRONLY);
    ssize_t n;

    if (fd < 0) {
        return -1;
    }
    n = write(fd, text, strlen(text));
    close(fd);
    return n == (ssize_t)strlen(text) ? 0 : -1;
}

static int read_text(const char *path, char *buf, size_t len) {
    int fd = open(path, O_RDONLY);
    ssize_t n;

    if (fd < 0) {
        return -1;
    }
    n = read(fd, buf, len - 1);
    close(fd);
    if (n < 0) {
        return -1;
    }
    buf[n] = '\0';
    return 0;
}

static unsigned long read_ulong_file(const char *path) {
    char buf[64];
    char *end = NULL;
    unsigned long value;

    if (read_text(path, buf, sizeof(buf)) != 0) {
        fail(path);
    }

    errno = 0;
    value = strtoul(buf, &end, 10);
    if (errno != 0 || end == buf) {
        printf("[FAIL] parse unsigned long from %s: %s\n", path, buf);
        exit(1);
    }
    return value;
}

static int file_contains_pid(const char *path, long pid) {
    char buf[512];
    char *save = NULL;
    char *line;

    if (read_text(path, buf, sizeof(buf)) != 0) {
        fail(path);
    }

    for (line = strtok_r(buf, "\n", &save); line != NULL;
         line = strtok_r(NULL, "\n", &save)) {
        if (strtol(line, NULL, 10) == pid) {
            return 1;
        }
    }
    return 0;
}

static void *worker_thread(void *arg) {
    (void)arg;
    while (atomic_load_explicit(&exec_ready, memory_order_acquire) == 0) {
        usleep(10000);
    }
    for (;;) {
        usleep(10000);
    }
    return NULL;
}

static void *exec_thread(void *arg) {
    (void)arg;
    atomic_store_explicit(&exec_ready, 1, memory_order_release);

    char *argv[] = {"/proc/self/exe", "exec-child", NULL};
    char *envp[] = {NULL};

    execve("/proc/self/exe", argv, envp);
    fail("execve from non-leader thread");
    return NULL;
}

static void run_after_exec_checks(void) {
    unsigned long parent_current = read_ulong_file(PARENT_CURRENT);
    unsigned long child_current = read_ulong_file(CHILD_CURRENT);
    pid_t pid;

    if (parent_current != 1 || child_current != 1) {
        printf("[FAIL] unexpected pids.current after de-thread: parent=%lu child=%lu\n",
               parent_current, child_current);
        exit(1);
    }
    if (!file_contains_pid(CHILD_PROCS, (long)getpid())) {
        printf("[FAIL] cgroup.procs does not contain exec survivor pid %ld\n", (long)getpid());
        exit(1);
    }

    if (write_text(PARENT_MAX, "1\n") != 0) {
        fail("set parent pids.max to 1");
    }
    errno = 0;
    pid = fork();
    if (pid == 0) {
        _exit(0);
    }
    if (pid > 0) {
        int status = 0;
        waitpid(pid, &status, 0);
        printf("[FAIL] fork unexpectedly bypassed parent pids.max after de-thread\n");
        exit(1);
    }
    if (errno != EAGAIN) {
        printf("[FAIL] fork failed with unexpected errno %d: %s\n", errno, strerror(errno));
        exit(1);
    }

    printf("[PASS] cgroup_exec_dethread_pids\n");
}

int main(int argc, char **argv) {
    pthread_t worker;
    pthread_t execer;

    if (argc > 1 && strcmp(argv[1], "exec-child") == 0) {
        run_after_exec_checks();
        return 0;
    }

    if (ensure_dir(PARENT) != 0) {
        fail("mkdir exec_dethread_parent");
    }
    if (ensure_dir(CHILD) != 0) {
        fail("mkdir exec_dethread_parent/child");
    }
    if (write_text(PARENT_MAX, "max\n") != 0) {
        fail("reset parent pids.max");
    }
    if (write_text(CHILD_PROCS, "0\n") != 0) {
        fail("move self to child cgroup");
    }

    if (pthread_create(&worker, NULL, worker_thread, NULL) != 0) {
        fail("pthread_create worker");
    }
    if (pthread_create(&execer, NULL, exec_thread, NULL) != 0) {
        fail("pthread_create execer");
    }

    for (;;) {
        usleep(100000);
    }
    return 0;
}
