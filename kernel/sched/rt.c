#include "rt.h"

struct sched_queue_rt sched_rt_ready_queue[MAX_CPU_NUM]; // 就绪队列

/**
 * @brief 调度函数
 *
 */
void sched_rt()
{
    cli();
    // 先选择需要调度的进程、再进行调度

    sti();
}

/**
 * @brief 将PCB加入就绪队列
 *
 * @param pcb
 */
void sched_rt_enqueue(struct rq *rq, struct process_control_block *pcb, int flags)
{
    struct sched_rt_entity *rt_se = &pcb->rt;
    // 将p的prio插入到对应链表中
    enque_rt_entity(rt_se, flags);
    // 如果进程不是当前在执行的进程，并且可以在其他rq上执行，则使用enqueue_pushable_task将进程按照优先级插入多链表中，
    if (!GET_CURRENT_PCB)
}

/**
 * @brief 从就绪队列中取出PCB
 *
 * @return struct process_control_block*
 */
struct process_control_block *sched_rt_dequeue()
{
}
/**
 * @brief 初始化RT进程调度器
 *
 */
void sched_rt_init()
{

    memset(&sched_rt_ready_queue, 0, sizeof(struct sched_queue_t) * MAX_CPU_NUM);
    for (int i = 0; i < MAX_CPU_NUM; ++i)
    {

        list_init(&sched_rt_ready_queue[i].proc_queue.list);
        sched_rt_ready_queue[i].count = 1; // 因为存在IDLE进程，因此为1
        sched_rt_ready_queue[i].cpu_exec_proc_jiffies = 5;
        sched_rt_ready_queue[i].proc_queue.virtual_runtime = 0x7fffffffffffffff;
    }
}

// struct process_control_block * pick_next_task_rt(struct rq *rq,struct process_control_block *prev,struct rq_flags *rf){
struct process_control_block *pick_next_task_rt(struct rq *rq)
{
    struct process_control_block *p = pick_task_rt(rq);

    if (p)
        set_next_task_rt(rq, p, true);

    return p;
}
static struct process_control_block *pick_task_rt(struct rq *rq)
{
    struct process_control_block *p;

    // if (!sched_rt_runnable(rq))
    //     return NULL;

    p = _pick_next_task_rt(rq);

    return p;
}
static struct process_control_block *_pick_next_task_rt(struct rq *rq)
{
    struct sched_rt_entity *rt_se;
    struct rt_rq *rt_rq = &rq->rt;

    rt_se = pick_next_rt_entity(rt_rq);
    // do
    // {
    //     rt_se = pick_next_rt_entity(rt_rq);
    //     BUG_ON(!rt_se);
    //     rt_rq = group_rt_rq(rt_se);
    // } while (rt_rq);

    return rt_task_of(rt_se);
}

static struct sched_rt_entity *pick_next_rt_entity(struct rt_rq *rt_rq)
{
    struct rt_prio_array *array = &rt_rq->active;
    struct sched_rt_entity *next = NULL;
    struct List *queue;
    int idx;

    // 此处查找链表中中下一个执行的entity
    idx = sched_find_first_bit(array->bitmap);
    // BUG_ON(idx >= MAX_RT_PRIO);

    queue = array->queue + idx;
    next = list_entry(queue->next, struct sched_rt_entity, run_list);

    return next;
}
static inline struct process_control_block *rt_task_of(struct sched_rt_entity *rt_se)
{
    return container_of(rt_se, struct process_control_block, rt);
}
/*
 * Adding/removing a task to/from a priority array:
 */

/**
 * @brief 将rt_se插入到进程优先级对应的链表中
 *
 * @param rq
 * @param p
 * @param flags
 */
static void enqueue_task_rt(struct rq *rq, struct process_control_block *p, int flags)
{
    struct sched_rt_entity *rt_se = &p->rt;

    enqueue_rt_entity(rt_se, flags);

    // if (!task_current(rq, p))
    //     enqueue_pushable_task(rq, p);
}

static void enqueue_rt_entity(struct sched_rt_entity *rt_se, unsigned int flags)
{
    struct rq *rq = rt_se->rt_rq->rq;
    __enqueue_rt_entity(rt_se, flags); // 将当前task enqueue到rt的rq中
}
static void __enqueue_rt_entity(struct sched_rt_entity *rt_se, unsigned int flags)
{
    struct rt_rq *rt_rq = rt_se->rt_rq;
    struct rt_prio_array *array = &rt_rq->active;
    struct List *queue = array->queue + rt_task_of(rt_se)->priority;

    list_append(&rt_se->run_list, queue);
    rt_se->on_list = 1;
    rt_se->on_rq = 1;
}
static inline struct process_control_block *rt_task_of(struct sched_rt_entity *rt_se)
{
    return container_of(rt_se, struct process_control_block, rt);
}

static void dequeue_task_rt(struct rq *rq, struct process_control_block *p, int flags)
{
    struct sched_rt_entity *rt_se = &p->rt;

    // update_curr_rt(rq);
    dequeue_rt_entity(rt_se, flags);

    // dequeue_pushable_task(rq, p);
}
static void dequeue_rt_entity(struct sched_rt_entity *rt_se, unsigned int flags)
{
    struct rq *rq = rt_se->rt_rq->rq;

    __dequeue_rt_entity(rt_se, flags);

    enqueue_top_rt_rq(&rq->rt);
}
static void __dequeue_rt_entity(struct sched_rt_entity *rt_se, unsigned int flags)
{
    struct rt_rq *rt_rq = rt_se->rt_rq;
    struct rt_prio_array *array = &rt_rq->active;
    if (rt_se->on_list)
        __delist_rt_entity(rt_se, array);

    rt_se->on_rq = 0;
}
static void __delist_rt_entity(struct sched_rt_entity *rt_se, struct rt_prio_array *array)
{
    list_del_init(&rt_se->run_list);
    rt_se->on_list = 0;
}
static void put_prev_task_rt(struct rq *rq, struct process_control_block *p)
{
    struct sched_rt_entity *rt_se = &p->rt;
    struct rt_rq *rt_rq = &rq->rt;
    // 这里没看懂
    // if (on_rt_rq(&p->rt))
    //     update_stats_wait_start_rt(rt_rq, rt_se);

    // update_curr_rt(rq);

    // update_rt_rq_load_avg(rq_clock_pelt(rq), rq, 1);

    /*
     * The previous task needs to be made eligible for pushing
     * if it is still active
     */
    if (on_rt_rq(&p->rt))
        enqueue_pushable_task(rq, p);
}
static inline int on_rt_rq(struct sched_rt_entity *rt_se)
{
    return rt_se->on_rq;
}
