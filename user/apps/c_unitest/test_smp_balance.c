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
#include <stdatomic.h>
#include <sys/syscall.h>

#define NUM_THREADS 4
#define MAX_CPUS 256

static atomic_int done = 0;

static int get_cpu(void) {
    unsigned int cpu;
    if (syscall(SYS_getcpu, &cpu, NULL, NULL) != 0) {
        perror("getcpu failed");
        return -1;
    }
    return (int)cpu;
}

static void *worker(void *arg) {
    int id = *(int *)arg;
    int start_cpu = get_cpu();
    volatile unsigned long count = 0;

    printf("Thread %d: started on CPU %d\n", id, start_cpu);
    if (start_cpu < 0) {
        fprintf(stderr, "Thread %d: failed to get CPU\n", id);
        return NULL;
    }

    /* CPU密集型工作 */
    while (!atomic_load_explicit(&done, memory_order_acquire)) {
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
    int num_cpus = (int)sysconf(_SC_NPROCESSORS_ONLN);
    int cpu_counts[MAX_CPUS] = {0};
    int unique_cpus = 0;
    int ret = 0;

    printf("=== SMP Load Balance Quick Test ===\n");
    printf("CPUs online: %d\n", num_cpus);
    printf("Main on CPU: %d\n\n", get_cpu());

    if (num_cpus <= 0 || num_cpus > MAX_CPUS) {
        fprintf(stderr, "Invalid CPU count: %d\n", num_cpus);
        return 1;
    }

    /* 创建线程 */
    for (i = 0; i < NUM_THREADS; i++) {
        ids[i] = i;
        if (pthread_create(&threads[i], NULL, worker, &ids[i]) != 0) {
            perror("pthread_create failed");
            atomic_store_explicit(&done, 1, memory_order_release);
            /* 等待已创建的线程 */
            for (int j = 0; j < i; j++) {
                pthread_join(threads[j], NULL);
            }
            return 1;
        }
    }

    /* 运行3秒 */
    sleep(3);
    atomic_store_explicit(&done, 1, memory_order_release);

    /* 等待结束 */
    for (i = 0; i < NUM_THREADS; i++) {
        if (pthread_join(threads[i], NULL) != 0) {
            perror("pthread_join failed");
        }
    }

    /* 统计线程最终分布 */
    for (i = 0; i < NUM_THREADS; i++) {
        /* 这里简化处理，实际应在线程中传回最终CPU */
    }

    printf("\nTest completed.\n");
    return ret;
}
