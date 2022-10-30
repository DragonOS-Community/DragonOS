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

struct sched_param
{
    int sched_priority;
};
struct sched_attr
{
    uint32_t size;

    uint32_t sched_policy;
    uint64_t sched_flags;

    /* SCHED_NORMAL, SCHED_BATCH */
    int32_t sched_nice;

    /* SCHED_FIFO, SCHED_RR */
    uint32_t sched_priority;

    /* SCHED_DEADLINE */
    uint64_t sched_runtime;
    uint64_t sched_deadline;
    uint64_t sched_period;

    /* Utilization hints */
    uint32_t sched_util_min;
    uint32_t sched_util_max;
};

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

