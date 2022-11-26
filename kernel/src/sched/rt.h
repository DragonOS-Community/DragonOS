
#pragma once

#include <common/glib.h>
#include <process/process.h>
#include "sched.h"

// #define RR_TIMESLICE       (100 * HZ / 1000)
#define RR_TIMESLICE       100
/**
 * @brief 初始化RT进程调度器
 *
 */
void sched_rt_init();
void init_rt_rq(struct rt_rq *rt_rq);
static struct sched_rt_entity *pick_next_rt_entity(struct rt_rq *rt_rq);
static struct process_control_block *_pick_next_task_rt(struct rq *rq);
static struct process_control_block *pick_task_rt(struct rq *rq);

struct process_control_block *pick_next_task_rt(struct rq *rq);

static inline struct process_control_block *rt_task_of(struct sched_rt_entity *rt_se);
static void __enqueue_rt_entity(struct sched_rt_entity *rt_se, unsigned int flags);
static void enqueue_rt_entity(struct sched_rt_entity *rt_se, unsigned int flags);

/**
 * @brief 将rt_se插入到进程优先级对应的链表中
 *
 * @param rq
 * @param p
 * @param flags
 */
static void enqueue_task_rt(struct rq *rq, struct process_control_block *p, int flags);

static void __delist_rt_entity(struct sched_rt_entity *rt_se, struct rt_prio_array *array);
static void __dequeue_rt_entity(struct sched_rt_entity *rt_se, unsigned int flags);
static void dequeue_rt_entity(struct sched_rt_entity *rt_se, unsigned int flags);
static void dequeue_task_rt(struct rq *rq, struct process_control_block *p, int flags);

/**
 * @brief 调度函数
 *
 */
void sched_rt();