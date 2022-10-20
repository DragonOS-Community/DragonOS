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
struct sched_param
{
	int sched_priority;
};
struct sched_attr
{
	unsigned int size;

	unsigned int sched_policy;
	unsigned long sched_flags;

	/* SCHED_NORMAL, SCHED_BATCH */
	signed int sched_nice;

	/* SCHED_FIFO, SCHED_RR */
	unsigned int sched_priority;

	/* SCHED_DEADLINE */
	unsigned long sched_runtime;
	unsigned long sched_deadline;
	unsigned long sched_period;

	/* Utilization hints */
	unsigned int sched_util_min;
	unsigned int sched_util_max;
};
static int __sched_setscheduler(struct process_control_block *p,
                                const struct sched_attr *attr, bool user, bool pi);
static int _sched_setscheduler(struct process_control_block *p, int policy,
                               const struct sched_param *param, bool check);
/**
 * sched_setscheduler -设置进程的policy
 * @p: 需要修改的pcb
 * @policy: 需要设置的policy
 * @param: structure containing the new RT priority. 目前没有用
 *
 *
 * Return: 成功返回0,否则返回-22
 *
 */
int sched_setscheduler(struct process_control_block *p, int policy,
                       const struct sched_param *param);
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