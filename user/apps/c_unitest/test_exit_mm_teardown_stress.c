#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static int fault_in_exit_mapping(void) {
    const size_t len = 64 * 4096;
    volatile unsigned char *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
        printf("[FAIL] mmap for exit stress failed: errno=%d(%s)\n",
               errno, strerror(errno));
        return -1;
    }

    for (size_t i = 0; i < len; i += 4096) {
        p[i] = (unsigned char)(i >> 12);
    }

    return 0;
}

static int run_one_child(void) {
    pid_t child = fork();
    if (child < 0) {
        printf("[FAIL] fork failed: errno=%d(%s)\n", errno, strerror(errno));
        return -1;
    }

    if (child == 0) {
        if (fault_in_exit_mapping() != 0) {
            _exit(10);
        }
        _exit(0);
    }

    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        printf("[FAIL] waitpid(%d) failed: errno=%d(%s)\n",
               child, errno, strerror(errno));
        return -1;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        printf("[FAIL] child %d exited abnormally: status=0x%x\n", child, status);
        return -1;
    }
    return 0;
}

int main(void) {
    int iterations = 128;
    const char *env = getenv("CAPSET_STRESS_ITERS");
    if (env && env[0]) {
        int parsed = atoi(env);
        if (parsed > 0) {
            iterations = parsed;
        }
    }

    for (int i = 0; i < iterations; ++i) {
        if (run_one_child() != 0) {
            printf("[FAIL] exit mm teardown stress failed at iteration %d/%d\n",
                   i + 1, iterations);
            return 1;
        }
    }

    printf("exit mm teardown stress passed: iterations=%d\n", iterations);
    return 0;
}
