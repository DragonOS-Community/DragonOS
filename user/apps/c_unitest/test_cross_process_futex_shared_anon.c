#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef FUTEX_WAIT
#define FUTEX_WAIT 0
#endif
#ifndef FUTEX_WAKE
#define FUTEX_WAKE 1
#endif

static long futex(uint32_t *uaddr, int futex_op, uint32_t val, const struct timespec *timeout) {
    return syscall(SYS_futex, uaddr, futex_op, val, timeout, NULL, 0);
}

int main() {
    size_t sz = getpagesize();
    uint32_t *shared = mmap(NULL, sz, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        perror("mmap");
        return 1;
    }

    // 初始值1，子进程在值为1时等待
    *shared = 1;

    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        return 1;
    }

    if (pid == 0) {
        // 子进程：等待父进程唤醒
        if (futex(shared, FUTEX_WAIT, 1, NULL) != 0) {
            perror("child futex_wait");
            _exit(1);
        }
        _exit(0);
    }

    // 父进程：给子进程一点时间进入wait
    struct timespec ts = { .tv_sec = 0, .tv_nsec = 50 * 1000 * 1000 };
    nanosleep(&ts, NULL);

    // 修改值并唤醒一个等待者
    __sync_fetch_and_add(shared, 1);
    long r = futex(shared, FUTEX_WAKE, 1, NULL);
    if (r != 1) {
        fprintf(stderr, "futex_wake returned %ld (errno=%d)\n", r, errno);
        return 2;
    }

    int status = 0;
    if (waitpid(pid, &status, 0) != pid) {
        perror("waitpid");
        return 3;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        fprintf(stderr, "child exit status=%d\n", status);
        return 4;
    }

    munmap(shared, sz);
    printf("ok\n");
    return 0;
}


