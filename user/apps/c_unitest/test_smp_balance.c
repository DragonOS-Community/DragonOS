/**
 * @file test_smp_balance.c
 * @brief 简单的SMP负载均衡验证程序
 *
 * 快速验证多核负载均衡是否工作
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <unistd.h>
#include <pthread.h>
#include <sys/syscall.h>

#define NUM_THREADS 4

static volatile int done = 0;

static int get_cpu(void) {
    unsigned int cpu;
    syscall(SYS_getcpu, &cpu, NULL, NULL);
    return (int)cpu;
}

static void *worker(void *arg) {
    int id = *(int *)arg;
    int start_cpu = get_cpu();
    volatile unsigned long count = 0;

    printf("Thread %d: started on CPU %d\n", id, start_cpu);

    /* CPU密集型工作 */
    while (!done) {
        for (int i = 0; i < 1000000; i++) {
            count += i;
        }
    }

    int end_cpu = get_cpu();
    printf("Thread %d: ended on CPU %d (count=%lu)\n", id, end_cpu, count);

    return NULL;
}

int main(void) {
    pthread_t threads[NUM_THREADS];
    int ids[NUM_THREADS];
    int i;

    printf("=== SMP Load Balance Quick Test ===\n");
    printf("CPUs online: %ld\n", sysconf(_SC_NPROCESSORS_ONLN));
    printf("Main on CPU: %d\n\n", get_cpu());

    /* 创建线程 */
    for (i = 0; i < NUM_THREADS; i++) {
        ids[i] = i;
        pthread_create(&threads[i], NULL, worker, &ids[i]);
    }

    /* 运行3秒 */
    sleep(3);
    done = 1;

    /* 等待结束 */
    for (i = 0; i < NUM_THREADS; i++) {
        pthread_join(threads[i], NULL);
    }

    printf("\nTest completed.\n");
    return 0;
}
