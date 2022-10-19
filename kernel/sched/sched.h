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