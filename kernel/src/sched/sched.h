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
#define MAX_RT_PRIO 100

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

/**
 * @brief RT调度类的优先级队列数据结构
 *
 */
struct rt_prio_array
{
    // TODO: 定义MAX_RT_PRIO为100
    struct List queue[MAX_RT_PRIO];
};
struct sched_entity
{
    unsigned int on_rq;
    unsigned long exec_start;
};
struct sched_rt_entity
{
    // 用于加入到优先级队列中
    struct List run_list;
    unsigned long timeout;
    unsigned short on_rq;   // 入队之后设置1
    unsigned short on_list; // 入队之后设置1
    /* rq on which this entity is (to be) queued: */
    struct rt_rq *rt_rq;
    struct sched_rt_entity *parent;
    struct sched_rt_entity *back;
    unsigned int time_slice; //针对RR调度策略的调度时隙
};
struct plist_head
{
    struct List node_list;
};
/**
 * @brief rt运行队列
 *
 */
struct rt_rq
{
    struct rt_prio_array active;
    unsigned int rt_nr_running; // rt队列中的任务数
    unsigned int rr_nr_running;
    struct rq *rq;
    struct plist_head pushable_tasks;
    unsigned long rt_time; //当前队列的累计运行时间
    unsigned long rt_runtime; //当前队列的单个周期内的最大运行时间
};
struct rq
{
    /* data */
    // struct cfs_rq cfs;
    struct rt_rq rt;
    // struct dl_rq dl;
};

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
