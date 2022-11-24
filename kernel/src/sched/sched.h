#pragma once

#include <common/glib.h>
#include <process/process.h>

/*
 * Scheduling policies
 */
#define SCHED_NORMAL 0
#define SCHED_FIFO 1
#define SCHED_RR 2
#define SCHED_BATCH 3
/* SCHED_ISO: reserved but not implemented yet */
#define SCHED_IDLE 5
#define SCHED_DEADLINE 6
#define SCHED_MAX_POLICY_NUM SCHED_DEADLINE

#define IS_VALID_SCHED_POLICY(_policy) ((_policy) > 0 && (_policy) <= SCHED_MAX_POLICY_NUM)

/**
 * @brief 根据结构体变量内某个成员变量member的基地址，计算出该结构体变量的基地址
 * @param ptr 指向结构体变量内的成员变量member的指针
 * @param type 成员变量所在的结构体
 * @param member 成员变量名
 *
 * 方法：使用ptr减去结构体内的偏移，得到结构体变量的基地址
 */
#define container_of(ptr, type, member)                                     \
    ({                                                                      \
        typeof(((type *)0)->member) *p = (ptr);                             \
        (type *)((unsigned long)p - (unsigned long)&(((type *)0)->member)); \
    })

static int __sched_setscheduler(struct process_control_block *p, const struct sched_attr *attr, bool user, bool pi);
static int _sched_setscheduler(struct process_control_block *p, int policy, const struct sched_param *param,
                               bool check);
/**
 * sched_setscheduler -设置进程的调度策略
 * @param p 需要修改的pcb
 * @param policy 需要设置的policy
 * @param param structure containing the new RT priority. 目前没有用
 *
 * @return 成功返回0,否则返回对应的错误码
 *
 */
int sched_setscheduler(struct process_control_block *p, int policy, const struct sched_param *param);
/**
 * @brief 包裹sched_enqueue(),将PCB加入就绪队列
 *
 * @param pcb
 */

int sched_gerscheduler(struct process_control_block *p, int policy, const struct sched_param *param);
void sched_enqueue(struct process_control_block *pcb);
/**
 * @brief 包裹sched_cfs()，调度函数
 *
 */
void sched();

void sched_init();

/**
 * @brief 当时钟中断到达时，更新时间片
 *
 */
void sched_update_jiffies();
