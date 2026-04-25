#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static volatile int worker_running = 1;
static volatile int exec_ready = 0;

static void *worker_thread(void *arg) {
    printf("[Worker] Started, TID=%ld\n", (long)syscall(SYS_gettid));
    fflush(stdout);

    int count = 0;
    while (worker_running) {
        count++;
        usleep(10000);

        if (count % 50 == 0) {
            printf("[Worker] Still running (count=%d)\n", count);
            fflush(stdout);
        }
    }

    printf("[Worker] Exiting\n");
    return NULL;
}

static void *exec_thread(void *arg) {
    printf("[Exec Thread] Started, TID=%ld\n", (long)syscall(SYS_gettid));
    fflush(stdout);

    usleep(500000);
    exec_ready = 1;

    printf("[Exec Thread] Calling execve...\n");
    fflush(stdout);

    char *argv[] = {"/proc/self/exe", "exec-child", NULL};
    char *envp[] = {NULL};

    execve("/proc/self/exe", argv, envp);

    perror("[Exec Thread] execve failed");
    return NULL;
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "exec-child") == 0) {
        printf("Exec succeeded! Worker should be terminated\n");
        fflush(stdout);
        return 0;
    }

    printf("=== Two-Thread Exec Test ===\n");
    printf("Main PID: %d\n\n", getpid());

    pthread_t worker;
    pthread_t exec_thr;

    if (pthread_create(&worker, NULL, worker_thread, NULL) != 0) {
        perror("pthread_create worker failed");
        return 1;
    }

    if (pthread_create(&exec_thr, NULL, exec_thread, NULL) != 0) {
        perror("pthread_create exec failed");
        return 1;
    }

    printf("[Main] Waiting for exec...\n");
    fflush(stdout);

    while (1) {
        usleep(100000);
        if (exec_ready) {
            printf("[Main] Exec starting, main should be terminated...\n");
            fflush(stdout);
        }
    }

    return 0;
}
