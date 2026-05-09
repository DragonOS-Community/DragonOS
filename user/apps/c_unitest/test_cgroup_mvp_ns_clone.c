#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef SYS_clone3
#define SYS_clone3 435
#endif

#ifndef CLONE_NEWCGROUP
#define CLONE_NEWCGROUP 0x02000000
#endif

#ifndef CLONE_INTO_CGROUP
#define CLONE_INTO_CGROUP 0x200000000ULL
#endif

/*
 * 本测试覆盖 cgroup v2 MVP 中 namespace 与 clone3 的联动路径：
 * 1) unshare(CLONE_NEWCGROUP) 后 /proc/self/cgroup 视图根发生变化。
 * 2) setns() 可以切回旧的 cgroup namespace。
 * 3) clone3(CLONE_INTO_CGROUP) 覆盖 bad-fd 与成功路径。
 * 4) 在不同 cgroup namespace 下，sibling 视图路径投影符合预期。
 */

struct clone_args_local {
    uint64_t flags;
    uint64_t pidfd;
    uint64_t child_tid;
    uint64_t parent_tid;
    uint64_t exit_signal;
    uint64_t stack;
    uint64_t stack_size;
    uint64_t tls;
    uint64_t set_tid;
    uint64_t set_tid_size;
    uint64_t cgroup;
};

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

static void *map_prot_none_page(void) {
    long page_size = sysconf(_SC_PAGESIZE);
    void *addr;

    if (page_size <= 0) {
        printf("[FAIL] invalid page size: %ld\n", page_size);
        exit(1);
    }

    addr = mmap(NULL, (size_t)page_size, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (addr == MAP_FAILED) {
        fail("mmap PROT_NONE");
    }

    return addr;
}

static void run_unshare_setns_case(void) {
    const char *grp = "/sys/fs/cgroup/mvp_setns_combo";
    const char *procs = "/sys/fs/cgroup/mvp_setns_combo/cgroup.procs";
    int old_ns_fd;
    char buf[256];

    if (ensure_dir(grp) != 0) {
        fail("mkdir mvp_setns_combo");
    }
    if (write_text(procs, "0\n") != 0) {
        fail("move self to mvp_setns_combo");
    }

    old_ns_fd = open("/proc/self/ns/cgroup", O_RDONLY);
    if (old_ns_fd < 0) {
        fail("open old cgroup ns fd");
    }

    if (unshare(CLONE_NEWCGROUP) != 0) {
        close(old_ns_fd);
        fail("unshare CLONE_NEWCGROUP");
    }

    if (read_text("/proc/self/cgroup", buf, sizeof(buf)) != 0) {
        close(old_ns_fd);
        fail("read /proc/self/cgroup after unshare");
    }
    if (strstr(buf, "0::/") == NULL) {
        printf("[FAIL] unexpected after unshare: %s\n", buf);
        close(old_ns_fd);
        exit(1);
    }

    if (setns(old_ns_fd, CLONE_NEWCGROUP) != 0) {
        close(old_ns_fd);
        fail("setns back to old cgroup ns");
    }
    close(old_ns_fd);

    if (read_text("/proc/self/cgroup", buf, sizeof(buf)) != 0) {
        fail("read /proc/self/cgroup after setns");
    }
    if (strstr(buf, "0::/mvp_setns_combo") == NULL) {
        printf("[FAIL] unexpected after setns: %s\n", buf);
        exit(1);
    }
}

static void run_clone3_into_cgroup_case(void) {
    const char *grp = "/sys/fs/cgroup/mvp_clone3_combo";
    int cgfd;
    struct clone_args_local bad = {0};
    struct clone_args_local good = {0};
    long ret;
    long child;
    int status = 0;

    if (ensure_dir(grp) != 0) {
        fail("mkdir mvp_clone3_combo");
    }
    cgfd = open(grp, O_RDONLY | O_DIRECTORY);
    if (cgfd < 0) {
        fail("open mvp_clone3_combo");
    }

    bad.flags = CLONE_INTO_CGROUP;
    bad.exit_signal = SIGCHLD;
    bad.cgroup = (uint64_t)123456;
    ret = syscall(SYS_clone3, &bad, sizeof(bad));
    if (ret != -1 || errno != EBADF) {
        printf("[FAIL] clone3 bad-fd path: ret=%ld errno=%d\n", ret, errno);
        close(cgfd);
        exit(1);
    }

    good.flags = CLONE_INTO_CGROUP;
    good.exit_signal = SIGCHLD;
    good.cgroup = (uint64_t)cgfd;
    child = syscall(SYS_clone3, &good, sizeof(good));
    if (child < 0) {
        close(cgfd);
        fail("clone3 into cgroup");
    }
    if (child == 0) {
        char cgbuf[256];
        if (read_text("/proc/self/cgroup", cgbuf, sizeof(cgbuf)) != 0) {
            _exit(2);
        }
        if (strstr(cgbuf, "0::/mvp_clone3_combo") == NULL) {
            _exit(3);
        }
        _exit(0);
    }

    if (waitpid((pid_t)child, &status, 0) < 0) {
        close(cgfd);
        fail("waitpid clone3 child");
    }
    close(cgfd);

    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        printf("[FAIL] clone3 child status=0x%x\n", status);
        exit(1);
    }
}

static void run_invalid_parent_settid_case(void) {
    const char *grp = "/sys/fs/cgroup/mvp_parent_settid_invalid";
    const char *procs = "/sys/fs/cgroup/mvp_parent_settid_invalid/cgroup.procs";
    const char *pids_current = "/sys/fs/cgroup/mvp_parent_settid_invalid/pids.current";
    void *bad_tid = map_prot_none_page();
    struct clone_args_local args = {0};
    unsigned long before;
    unsigned long after;
    long child;
    int status = 0;

    if (ensure_dir(grp) != 0) {
        fail("mkdir mvp_parent_settid_invalid");
    }
    if (write_text(procs, "0\n") != 0) {
        fail("move self to mvp_parent_settid_invalid");
    }

    before = read_ulong_file(pids_current);

    args.flags = CLONE_PARENT_SETTID;
    args.exit_signal = SIGCHLD;
    args.parent_tid = (uint64_t)(uintptr_t)bad_tid;

    child = syscall(SYS_clone3, &args, sizeof(args));
    if (child < 0) {
        fail("clone3 invalid parent_tid");
    }
    if (child == 0) {
        _exit(0);
    }

    if (waitpid((pid_t)child, &status, 0) < 0) {
        fail("waitpid invalid parent_tid child");
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        printf("[FAIL] invalid parent_tid child status=0x%x\n", status);
        exit(1);
    }

    after = read_ulong_file(pids_current);
    if (after != before) {
        printf("[FAIL] invalid parent_tid leaked pids.current: before=%lu after=%lu\n", before,
               after);
        exit(1);
    }
    if (file_contains_pid(procs, child)) {
        printf("[FAIL] invalid parent_tid leaked child pid %ld into cgroup.procs\n", child);
        exit(1);
    }

    if (munmap(bad_tid, (size_t)sysconf(_SC_PAGESIZE)) != 0) {
        fail("munmap invalid parent_tid page");
    }
}

static void run_invalid_child_settid_case(void) {
    const char *grp = "/sys/fs/cgroup/mvp_child_settid_invalid";
    const char *procs = "/sys/fs/cgroup/mvp_child_settid_invalid/cgroup.procs";
    const char *pids_current = "/sys/fs/cgroup/mvp_child_settid_invalid/pids.current";
    void *bad_tid = map_prot_none_page();
    struct clone_args_local args = {0};
    unsigned long before;
    unsigned long after;
    long child;
    int status = 0;

    if (ensure_dir(grp) != 0) {
        fail("mkdir mvp_child_settid_invalid");
    }
    if (write_text(procs, "0\n") != 0) {
        fail("move self to mvp_child_settid_invalid");
    }

    before = read_ulong_file(pids_current);

    args.flags = CLONE_CHILD_SETTID;
    args.exit_signal = SIGCHLD;
    args.child_tid = (uint64_t)(uintptr_t)bad_tid;

    child = syscall(SYS_clone3, &args, sizeof(args));
    if (child < 0) {
        fail("clone3 invalid child_tid");
    }
    if (child == 0) {
        _exit(0);
    }

    if (waitpid((pid_t)child, &status, 0) < 0) {
        fail("waitpid invalid child_tid child");
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        printf("[FAIL] invalid child_tid child status=0x%x\n", status);
        exit(1);
    }

    after = read_ulong_file(pids_current);
    if (after != before) {
        printf("[FAIL] invalid child_tid leaked pids.current: before=%lu after=%lu\n", before,
               after);
        exit(1);
    }
    if (file_contains_pid(procs, child)) {
        printf("[FAIL] invalid child_tid leaked child pid %ld into cgroup.procs\n", child);
        exit(1);
    }

    if (munmap(bad_tid, (size_t)sysconf(_SC_PAGESIZE)) != 0) {
        fail("munmap invalid child_tid page");
    }
}

static void run_sibling_view_case(void) {
    const char *ga = "/sys/fs/cgroup/mvp_view_a_combo";
    const char *gb = "/sys/fs/cgroup/mvp_view_b_combo";
    const char *pa = "/sys/fs/cgroup/mvp_view_a_combo/cgroup.procs";
    const char *pb = "/sys/fs/cgroup/mvp_view_b_combo/cgroup.procs";
    int pipefd[2];
    pid_t pid;
    char ok = 0;
    char path[64];
    char buf[256];

    if (ensure_dir(ga) != 0 || ensure_dir(gb) != 0) {
        fail("mkdir sibling cgroups");
    }
    if (write_text(pa, "0\n") != 0) {
        fail("move parent to mvp_view_a_combo");
    }

    if (pipe(pipefd) != 0) {
        fail("pipe for sibling case");
    }

    pid = fork();
    if (pid < 0) {
        fail("fork sibling child");
    }
    if (pid == 0) {
        close(pipefd[0]);
        if (write_text(pb, "0\n") != 0) {
            _exit(2);
        }
        ok = '1';
        if (write(pipefd[1], &ok, 1) != 1) {
            _exit(3);
        }
        close(pipefd[1]);
        pause();
        _exit(0);
    }

    close(pipefd[1]);
    if (read(pipefd[0], &ok, 1) != 1 || ok != '1') {
        close(pipefd[0]);
        kill(pid, SIGKILL);
        waitpid(pid, NULL, 0);
        printf("[FAIL] sibling child setup failed\n");
        exit(1);
    }
    close(pipefd[0]);

    if (unshare(CLONE_NEWCGROUP) != 0) {
        kill(pid, SIGKILL);
        waitpid(pid, NULL, 0);
        fail("unshare before sibling proc view check");
    }

    snprintf(path, sizeof(path), "/proc/%d/cgroup", (int)pid);
    if (read_text(path, buf, sizeof(buf)) != 0) {
        kill(pid, SIGKILL);
        waitpid(pid, NULL, 0);
        fail("read sibling /proc/<pid>/cgroup");
    }
    if (strstr(buf, "0::/../mvp_view_b_combo") == NULL) {
        printf("[FAIL] unexpected sibling projection: %s\n", buf);
        kill(pid, SIGKILL);
        waitpid(pid, NULL, 0);
        exit(1);
    }

    kill(pid, SIGKILL);
    waitpid(pid, NULL, 0);
}

int main(void) {
    run_unshare_setns_case();
    run_clone3_into_cgroup_case();
    run_invalid_parent_settid_case();
    run_invalid_child_settid_case();
    run_sibling_view_case();
    printf("[PASS] cgroup_mvp_ns_clone\n");
    return 0;
}
