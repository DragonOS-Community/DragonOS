#pragma once

#include <common/numa.h>
#include <process/proc-types.h>
#include <common/err.h>
#include <process/process.h>

/**
 * @brief kthread信息
 * 该结构体将会绑定到pcb的worker_private中
 */
struct kthread_info_t
{
    uint64_t flags;
    uint32_t cpu;
    int result;
    int (*thread_fn)(void *);
    void *data;
    // todo: 将这里改为completion机制
    bool exited;     // 是否已退出
    char *full_name; // 内核线程的名称
};

struct process_control_block *kthread_create_on_node(int (*thread_fn)(void *data),
                                                     void *data,
                                                     int node,
                                                     const char name_fmt[], ...);
/**
 * @brief 在当前结点上创建一个内核线程
 *
 * @param thread_fn 该内核线程要执行的函数
 * @param data 传递给 thread_fn 的参数数据
 * @param name_fmt printf-style format string for the thread name
 * @param arg name_fmt的参数
 *
 * 请注意，该宏会创建一个内核线程，并将其设置为停止状态
 */
#define kthread_create(thread_fn, data, name_fmt, arg...) \
    kthread_create_on_node(thread_fn, data, NUMA_NO_NODE, name_fmt, ##arg)

/**
 * @brief 创建内核线程，并将其唤醒
 * 
 * @param thread_fn 该内核线程要执行的函数
 * @param data 传递给 thread_fn 的参数数据
 * @param name_fmt printf-style format string for the thread name
 * @param arg name_fmt的参数
 */
#define kthread_run(thread_fn, data, name_fmt, ...)                                                          \
    ({                                                                                                       \
        struct process_control_block *__kt = kthread_create(thread_fn, data, name_fmt, ##__VA_ARGS__); \
        if (!IS_ERR(__kt))                                                                                   \
            process_wakeup(__kt);                                                                            \
        __kt;                                                                                                \
    })

/**
 * @brief 向kthread发送停止信号，请求其结束
 * 
 * @param pcb 内核线程的pcb
 * @return int 错误码
 */
int kthread_stop(struct process_control_block * pcb);

/**
 * @brief 内核线程调用该函数，检查自身的标志位，判断自己是否应该执行完任务后退出
 * 
 * @return true 内核线程应该退出
 * @return false 无需退出
 */
bool kthread_should_stop(void);

/**
 * @brief 让当前内核线程退出，并返回result参数给kthread_stop()函数
 * 
 * @param result 返回值
 */
void kthread_exit(long result);

/**
 * @brief 初始化kthread机制(只应被process_init调用)
 * 
 * @return int 错误码
 */
int kthread_mechanism_init();

/**
 * @brief 设置pcb中的worker_private字段（只应被设置一次）
 *
 * @param pcb pcb
 * @return bool 成功或失败
 */
bool kthread_set_worker_private(struct process_control_block *pcb);

/**
 * @brief 获取pcb中的kthread结构体
 *
 * @param pcb pcb
 * @return struct kthread* kthread信息结构体
 */
struct kthread_info_t *to_kthread(struct process_control_block *pcb);

/**
 * @brief 释放pcb指向的worker private
 * 
 * @param pcb 要释放的pcb
 */
void free_kthread_struct(struct process_control_block *pcb);