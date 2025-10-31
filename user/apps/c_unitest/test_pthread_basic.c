#include <stdio.h>
#include <pthread.h>
#include <unistd.h>
#include <stdlib.h>
#include <errno.h>

// 定义要创建的子线程数量
#define NUM_THREADS 50

// 全局共享变量
static int shared_counter = 0;
static pthread_mutex_t counter_mutex = PTHREAD_MUTEX_INITIALIZER;

// 全局健壮锁，用于测试 robust mutex
static pthread_mutex_t robust_mutex;

// 子线程函数
void *thread_function(void *arg) {
    int thread_id = *(int *)arg;
    
    printf("[Thread %d] 子线程开始运行\n", thread_id);
    printf("[Thread %d] 子线程 ID: %lu, PID: %d\n", thread_id, pthread_self(), getpid());
    
    // 测试普通互斥锁 - 触发 futex
    printf("[Thread %d] 尝试获取普通互斥锁...\n", thread_id);
    pthread_mutex_lock(&counter_mutex);
    printf("[Thread %d] 获取普通互斥锁成功，进入临界区\n", thread_id);

    // 临界区操作
    int old_value = shared_counter;
    // usleep(100000); // 延长临界区时间，增加竞争
    shared_counter++;
    printf("[Thread %d] 共享计数器: %d -> %d\n", thread_id, old_value, shared_counter);
    
    pthread_mutex_unlock(&counter_mutex);
    printf("[Thread %d] 释放普通互斥锁\n", thread_id);
    
    // 测试健壮锁 - 可以处理持有者异常退出的情况
    printf("[Thread %d] 尝试获取健壮锁...\n", thread_id);
    int ret = pthread_mutex_lock(&robust_mutex);
    if (ret == 0) {
        printf("[Thread %d] 获取健壮锁成功\n", thread_id);
        // 模拟一些工作
        // usleep(50000);
        pthread_mutex_unlock(&robust_mutex);
        printf("[Thread %d] 释放健壮锁\n", thread_id);
    } else if (ret == EOWNERDEAD) {
        printf("[Thread %d] 检测到健壮锁持有者已死亡，尝试恢复\n", thread_id);
        pthread_mutex_consistent(&robust_mutex);
        pthread_mutex_unlock(&robust_mutex);
        printf("[Thread %d] 健壮锁已恢复并释放\n", thread_id);
    } else {
        printf("[Thread %d] 获取健壮锁失败，错误码: %d\n", thread_id, ret);
    }
    
    // 模拟一些工作
    printf("[Thread %d] 正在执行工作...\n", thread_id);
    // sleep(2);
    
    printf("[Thread %d] 工作完成，准备退出\n", thread_id);
    
    // 返回值
    int *result = malloc(sizeof(int));
    *result = thread_id * 100;
    
    printf("[Thread %d] 子线程退出，返回值: %d\n", thread_id, *result);
    // 直接返回，效果与 pthread_exit(result) 相同
    return result;
}

int main() {
    pthread_t threads[NUM_THREADS];
    int thread_args[NUM_THREADS];
    int ret;
    void *thread_result;
    pthread_mutexattr_t attr;
    
    printf("=== pthread_create 和 pthread_join 测试程序 ===\n");
    printf("[Main] 主线程开始，线程 ID: %lu\n", pthread_self());
    
    // 初始化健壮锁
    printf("[Main] 初始化健壮锁...\n");
    pthread_mutexattr_init(&attr);
    pthread_mutexattr_setrobust(&attr, PTHREAD_MUTEX_ROBUST);
    pthread_mutexattr_setpshared(&attr, PTHREAD_PROCESS_PRIVATE);
    ret = pthread_mutex_init(&robust_mutex, &attr);
    if (ret != 0) {
        printf("[Main] 错误: 健壮锁初始化失败，返回值 = %d\n", ret);
        return 1;
    }
    pthread_mutexattr_destroy(&attr);
    printf("[Main] 健壮锁初始化成功\n");
    
    printf("[Main] 准备创建 %d 个子线程\n", NUM_THREADS);
    
    // 创建多个子线程
    for (int i = 0; i < NUM_THREADS; i++) {
        thread_args[i] = i + 1;
        printf("[Main] 正在创建子线程 %d...\n", thread_args[i]);
        ret = pthread_create(&threads[i], NULL, thread_function, &thread_args[i]);
        if (ret != 0) {
            printf("[Main] 错误: pthread_create 失败，线程 %d，返回值 = %d\n", i + 1, ret);
            return 1;
        }
        printf("[Main] 子线程 %d 创建成功，线程句柄: %lu\n", thread_args[i], threads[i]);
    }
    
    printf("[Main] 所有子线程创建完成\n");
    printf("[Main] 主线程继续执行自己的工作...\n");
    
    // 主线程也做一些工作
    // !!!!看这里！只要开启sleep的话，等待所有子线程都返回了再join,就不会报错 can not find nearest vma
    // sleep(1);
    printf("[Main] 主线程工作完成，等待所有子线程结束...\n");
    
    // 等待所有子线程结束
    for (int i = 0; i < NUM_THREADS; i++) {
        printf("[Main] 调用 pthread_join 等待子线程 %d...\n", thread_args[i]);
        ret = pthread_join(threads[i], &thread_result);
        if (ret != 0) {
            printf("[Main] 错误: pthread_join 失败，线程 %d，返回值 = %d\n", thread_args[i], ret);
            return 1;
        }
        
        printf("[Main] 子线程 %d pthread_join 成功返回\n", thread_args[i]);
        
        if (thread_result != NULL) {
            int result_value = *(int *)thread_result;
            printf("[Main] 子线程 %d 返回值: %d\n", thread_args[i], result_value);
            free(thread_result);
        } else {
            printf("[Main] 子线程 %d 返回值为 NULL\n", thread_args[i]);
        }
    }
    
    printf("[Main] 所有线程已结束，程序退出\n");
    printf("[Main] 最终共享计数器值: %d (期望值: %d)\n", shared_counter, NUM_THREADS);
    
    // 清理资源
    pthread_mutex_destroy(&counter_mutex);
    pthread_mutex_destroy(&robust_mutex);
    printf("[Main] 互斥锁已销毁\n");
    
    printf("=== 测试完成 ===\n");
    
    return 0;
}

