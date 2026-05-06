/**
 * @file test_load_balance.c
 * @brief 多核负载均衡功能测试程序
 *
 * 测试场景：
 * 1. 创建多个CPU密集型任务，验证它们是否分布在不同CPU上
 * 2. 测试任务唤醒时的CPU选择
 * 3. 测试负载均衡的周期性迁移（尚未实现）
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>

#define NUM_WORKERS 4
#define WORK_ITERATIONS 10000000
#define TEST_DURATION_SEC 5
#define MAX_CPUS 256

/* 用于统计的结构体 */
typedef struct {
    int thread_id;
    int initial_cpu;
    int final_cpu;
    int cpu_changes;
    unsigned long iterations;
} worker_stats_t;

/* 全局变量 */
static atomic_int running = 1;
static worker_stats_t stats[NUM_WORKERS];
static pthread_mutex_t print_mutex = PTHREAD_MUTEX_INITIALIZER;

/**
 * 获取当前线程运行的CPU ID
 */
static int get_current_cpu(void) {
    /* 使用 getcpu 系统调用 */
    unsigned int cpu, node;
    if (syscall(SYS_getcpu, &cpu, &node, NULL) == 0) {
        return (int)cpu;
    }
    return -1;
}

/**
 * CPU密集型工作函数
 * 执行一些计算密集的操作
 */
static unsigned long cpu_intensive_work(unsigned long iterations) {
    volatile unsigned long result = 0;
    for (unsigned long i = 0; i < iterations; ++i) {
        result += i * i;
        result ^= (result >> 3);
        result += (result << 5);
        __asm__ __volatile__("" : : "r"(result) : "memory");
    }
    return result;
}

/**
 * 工作线程函数
 */
static void *worker_thread(void *arg) {
    int thread_id = *(int *)arg;
    int last_cpu = -1;
    int current_cpu;

    stats[thread_id].thread_id = thread_id;
    stats[thread_id].cpu_changes = 0;
    stats[thread_id].iterations = 0;

    /* 记录初始CPU */
    stats[thread_id].initial_cpu = get_current_cpu();
    last_cpu = stats[thread_id].initial_cpu;

    pthread_mutex_lock(&print_mutex);
    printf("[Thread %d] Started on CPU %d\n", thread_id, stats[thread_id].initial_cpu);
    pthread_mutex_unlock(&print_mutex);

    /* 执行CPU密集型工作 */
    while (atomic_load_explicit(&running, memory_order_acquire)) {
        cpu_intensive_work(100000);
        stats[thread_id].iterations++;

        /* 检查是否发生了CPU迁移 */
        current_cpu = get_current_cpu();
        if (current_cpu != last_cpu && last_cpu != -1) {
            stats[thread_id].cpu_changes++;
            pthread_mutex_lock(&print_mutex);
            printf("[Thread %d] Migrated from CPU %d to CPU %d\n",
                   thread_id, last_cpu, current_cpu);
            pthread_mutex_unlock(&print_mutex);
            last_cpu = current_cpu;
        }
    }

    stats[thread_id].final_cpu = get_current_cpu();

    pthread_mutex_lock(&print_mutex);
    printf("[Thread %d] Finished on CPU %d (iterations: %lu, migrations: %d)\n",
           thread_id, stats[thread_id].final_cpu,
           stats[thread_id].iterations, stats[thread_id].cpu_changes);
    pthread_mutex_unlock(&print_mutex);

    return NULL;
}

/**
 * 测试1: 多线程负载分布测试
 * 创建多个CPU密集型线程，验证它们是否分布在不同CPU上
 */
static int test_load_distribution(void) {
    pthread_t threads[NUM_WORKERS];
    int thread_ids[NUM_WORKERS];
    int i;
    int cpu_usage[MAX_CPUS] = {0};
    int unique_cpus = 0;
    int num_cpus = (int)sysconf(_SC_NPROCESSORS_ONLN);
    if (num_cpus <= 0 || num_cpus > MAX_CPUS) {
        fprintf(stderr, "Invalid CPU count: %d\n", num_cpus);
        return -1;
    }

    printf("\n========================================\n");
    printf("Test 1: Load Distribution Test\n");
    printf("========================================\n");
    printf("Creating %d CPU-intensive threads...\n\n", NUM_WORKERS);

    atomic_store_explicit(&running, 1, memory_order_release);
    memset(stats, 0, sizeof(stats));

    /* 创建工作线程 */
    for (i = 0; i < NUM_WORKERS; ++i) {
        thread_ids[i] = i;
        if (pthread_create(&threads[i], NULL, worker_thread, &thread_ids[i]) != 0) {
            perror("pthread_create failed");
            return -1;
        }
    }

    /* 运行一段时间 */
    printf("Running for %d seconds...\n\n", TEST_DURATION_SEC);
    sleep(TEST_DURATION_SEC);

    /* 停止所有线程 */
    atomic_store_explicit(&running, 0, memory_order_release);

    /* 等待所有线程结束 */
    for (i = 0; i < NUM_WORKERS; ++i) {
        if (pthread_join(threads[i], NULL) != 0) {
            perror("pthread_join failed");
        }
    }

    /* 统计结果 */
    printf("\n--- Summary ---\n");
    for (i = 0; i < NUM_WORKERS; ++i) {
        printf("Thread %d: initial_cpu=%d, final_cpu=%d, migrations=%d\n",
               i, stats[i].initial_cpu, stats[i].final_cpu, stats[i].cpu_changes);

        if (stats[i].final_cpu >= 0 && stats[i].final_cpu < num_cpus) {
            cpu_usage[stats[i].final_cpu]++;
        }
    }

    /* 计算使用了多少个不同的CPU */
    for (i = 0; i < num_cpus; ++i) {
        if (cpu_usage[i] > 0) {
            unique_cpus++;
        }
    }

    printf("\nUnique CPUs used: %d (out of %d online)\n", unique_cpus, num_cpus);

    if (unique_cpus > 1) {
        printf("PASS: Tasks are distributed across multiple CPUs\n");
        return 0;
    } else if (num_cpus == 1) {
        printf("INFO: Single-core system, distribution test not applicable\n");
        return 0;
    } else {
        printf("FAIL: All tasks stayed on a single CPU on a %d-core system\n", num_cpus);
        return 1;
    }
}

/**
 * 测试2: 混合负载测试
 * 创建CPU密集型和IO密集型任务的混合
 */
static void *mixed_worker(void *arg) {
    int thread_id = *(int *)arg;
    int is_cpu_bound = (thread_id % 2 == 0);
    int cpu_changes = 0;
    int last_cpu = -1;
    int current_cpu;
    int iterations = 0;

    printf("[Mixed %d] Started on CPU %d (%s)\n",
           thread_id, get_current_cpu(),
           is_cpu_bound ? "CPU-bound" : "IO-bound");

    while (atomic_load_explicit(&running, memory_order_acquire) && iterations < 20) {
        if (is_cpu_bound) {
            /* CPU密集型工作 */
            cpu_intensive_work(500000);
        } else {
            /* IO密集型工作（模拟） */
            usleep(50000); /* 50ms */
        }

        iterations++;
        current_cpu = get_current_cpu();
        if (last_cpu != -1 && current_cpu != last_cpu) {
            cpu_changes++;
        }
        last_cpu = current_cpu;
    }

    printf("[Mixed %d] Finished on CPU %d (iterations=%d, migrations=%d)\n",
           thread_id, get_current_cpu(), iterations, cpu_changes);

    return NULL;
}

static int test_mixed_workload(void) {
    pthread_t threads[NUM_WORKERS];
    int thread_ids[NUM_WORKERS];
    int i;

    printf("\n========================================\n");
    printf("Test 2: Mixed Workload Test\n");
    printf("========================================\n");
    printf("Creating %d mixed threads (CPU-bound and IO-bound)...\n\n", NUM_WORKERS);

    atomic_store_explicit(&running, 1, memory_order_release);

    /* 创建工作线程 */
    for (i = 0; i < NUM_WORKERS; ++i) {
        thread_ids[i] = i;
        if (pthread_create(&threads[i], NULL, mixed_worker, &thread_ids[i]) != 0) {
            perror("pthread_create failed");
            return -1;
        }
    }

    /* 等待所有线程结束 */
    for (i = 0; i < NUM_WORKERS; ++i) {
        if (pthread_join(threads[i], NULL) != 0) {
            perror("pthread_join failed");
        }
    }

    printf("\nPASS: Mixed workload test completed\n");
    return 0;
}

/**
 * 测试3: 进程fork负载均衡测试
 * 创建多个子进程，验证它们是否分布在不同CPU上
 */
static int test_fork_balancing(void) {
    pid_t pids[NUM_WORKERS];
    int i;
    int status;
    int initial_cpus[NUM_WORKERS];
    int num_cpus = (int)sysconf(_SC_NPROCESSORS_ONLN);
    if (num_cpus <= 0 || num_cpus > MAX_CPUS) {
        fprintf(stderr, "Invalid CPU count: %d\n", num_cpus);
        return -1;
    }

    printf("\n========================================\n");
    printf("Test 3: Fork Load Balancing Test\n");
    printf("========================================\n");
    printf("Forking %d child processes...\n\n", NUM_WORKERS);

    for (i = 0; i < NUM_WORKERS; ++i) {
        pids[i] = fork();
        if (pids[i] < 0) {
            perror("fork failed");
            return -1;
        } else if (pids[i] == 0) {
            /* 子进程 */
            int my_cpu = get_current_cpu();
            printf("[Child %d] PID=%d, running on CPU %d\n", i, getpid(), my_cpu);

            /* 做一些CPU密集型工作 */
            cpu_intensive_work(WORK_ITERATIONS);

            int final_cpu = get_current_cpu();
            printf("[Child %d] PID=%d, finished on CPU %d\n", i, getpid(), final_cpu);

            exit(my_cpu); /* 返回初始CPU作为退出码 */
        }
    }

    /* 父进程等待所有子进程 */
    for (i = 0; i < NUM_WORKERS; ++i) {
        if (waitpid(pids[i], &status, 0) < 0) {
            perror("waitpid failed");
            initial_cpus[i] = -1;
            continue;
        }
        if (WIFEXITED(status)) {
            initial_cpus[i] = WEXITSTATUS(status);
        } else {
            initial_cpus[i] = -1;
        }
    }

    /* 分析结果 */
    printf("\n--- Summary ---\n");
    int cpu_count[MAX_CPUS] = {0};
    int unique_cpus = 0;

    for (i = 0; i < NUM_WORKERS; ++i) {
        printf("Child %d: initial CPU = %d\n", i, initial_cpus[i]);
        if (initial_cpus[i] >= 0 && initial_cpus[i] < num_cpus) {
            cpu_count[initial_cpus[i]]++;
        }
    }

    for (i = 0; i < num_cpus; ++i) {
        if (cpu_count[i] > 0) {
            unique_cpus++;
        }
    }

    printf("\nUnique CPUs used by children: %d (out of %d online)\n", unique_cpus, num_cpus);

    if (unique_cpus > 1) {
        printf("PASS: Child processes are distributed across multiple CPUs\n");
        return 0;
    } else if (num_cpus == 1) {
        printf("INFO: Single-core system, distribution test not applicable\n");
        return 0;
    } else {
        printf("FAIL: All children stayed on a single CPU on a %d-core system\n", num_cpus);
        return 1;
    }
}

/**
 * 打印系统信息
 */
static void print_system_info(void) {
    int num_cpus;
    int current_cpu;

    printf("========================================\n");
    printf("DragonOS Load Balancing Test Suite\n");
    printf("========================================\n\n");

    /* 获取CPU数量 */
    num_cpus = (int)sysconf(_SC_NPROCESSORS_ONLN);
    if (num_cpus > 0) {
        printf("Number of online CPUs: %d\n", num_cpus);
    } else {
        printf("Could not determine number of CPUs\n");
    }

    current_cpu = get_current_cpu();
    printf("Current CPU: %d\n", current_cpu);
    printf("Test PID: %d\n", getpid());
    printf("\n");
}

int main(int argc, char *argv[]) {
    int result = 0;

    (void)argc;
    (void)argv;

    print_system_info();

    /* 运行所有测试 */
    if (test_load_distribution() != 0) {
        result = 1;
    }

    if (test_mixed_workload() != 0) {
        result = 1;
    }

    if (test_fork_balancing() != 0) {
        result = 1;
    }

    printf("\n========================================\n");
    if (result == 0) {
        printf("All tests completed successfully!\n");
    } else {
        printf("Some tests failed!\n");
    }
    printf("========================================\n");

    return result;
}
