#include "rt.h"

struct sched_queue_rt sched_rt_ready_queue[MAX_CPU_NUM]; // 就绪队列


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
void init_rt_rq(struct rt_rq *rt_rq)
{
    struct rt_prio_array *array;
    int i;

    array = &rt_rq->active;
    for (i = 0; i < MAX_RT_PRIO; i++)
    {
        list_init(array->queue + i);
    }
    /* We start is dequeued state, because no RT tasks are queued */
    rt_rq->rt_queued = 0;

    rt_rq->rt_time = 0;
    rt_rq->rt_throttled = 0;
    rt_rq->rt_runtime = 0;
    raw_spin_lock_init(&rt_rq->rt_runtime_lock);
}
static struct sched_rt_entity *pick_next_rt_entity(struct rt_rq *rt_rq)
{
    struct rt_prio_array *array = &rt_rq->active;
    struct sched_rt_entity *next = NULL;
    struct List *queue;
    int idx;

    // 此处查找链表中中下一个执行的entity
    // TODO :不适用bitmap时如何找到对应的list
    // idx = sched_find_first_bit(array->bitmap);
    for (int i = 0; i < MAX_CPU_NUM; i++)
    {
        if (array->queue[i] != NULL)
        {
            queue = array->queue[i];
            break;
        }
    }
    if (queue == NULL)
    {
        return NULL;
    }
    // 获取当前的entry
    next = list_entry(queue->next, struct sched_rt_entity, run_list);

    return next;
}
static struct process_control_block *_pick_next_task_rt(struct rq *rq)
{
    struct sched_rt_entity *rt_se;
    struct rt_rq *rt_rq = &rq->rt;
    // 从rt_rq中找优先级最高且最先入队的task
    rt_se = pick_next_rt_entity(rt_rq);

    return rt_task_of(rt_se);
}
static struct process_control_block *pick_task_rt(struct rq *rq)
{
    struct process_control_block *p;
    // TODO:如果队列中元素为空，则返回null，

    p = _pick_next_task_rt(rq);

    return p;
}

struct process_control_block *pick_next_task_rt(struct rq *rq)
{
    struct process_control_block *p = pick_task_rt(rq);
    return p;
}

static inline struct process_control_block *rt_task_of(struct sched_rt_entity *rt_se)
{
    return container_of(rt_se, struct process_control_block, rt);
}
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

static void enqueue_rt_entity(struct sched_rt_entity *rt_se, unsigned int flags)
{
    struct rq *rq = rt_se->rt_rq->rq;
    __enqueue_rt_entity(rt_se, flags); // 将当前task enqueue到rt的rq中
}

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

static void __delist_rt_entity(struct sched_rt_entity *rt_se, struct rt_prio_array *array)
{
    list_del_init(&rt_se->run_list);
    rt_se->on_list = 0;
}
static void __dequeue_rt_entity(struct sched_rt_entity *rt_se, unsigned int flags)
{
    struct rt_rq *rt_rq = rt_se->rt_rq;
    struct rt_prio_array *array = &rt_rq->active;
    if (rt_se->on_list)
        __delist_rt_entity(rt_se, array);

    rt_se->on_rq = 0;
}
static void dequeue_rt_entity(struct sched_rt_entity *rt_se, unsigned int flags)
{
    struct rq *rq = rt_se->rt_rq->rq;

    __dequeue_rt_entity(rt_se, flags);

    enqueue_top_rt_rq(&rq->rt);
}
static void dequeue_task_rt(struct rq *rq, struct process_control_block *p, int flags)
{
    struct sched_rt_entity *rt_se = &p->rt;

    // update_curr_rt(rq);
    dequeue_rt_entity(rt_se, flags);

    // dequeue_pushable_task(rq, p);
}


/**
 * @brief 调度函数
 *
 */
void sched_rt()
{
    cli();
    // 先选择需要调度的进程、再进行调度
    current_pcb->flags &= ~PF_NEED_SCHED;
    // 如果是fifo策略，则可以一直占有cpu直到有优先级更高的任务就绪(即使优先级相同也不行)或者主动放弃(等待资源)
    if (current_pcb->policy == SCHED_FIFO)
    {   
        current_pcb->
        struct process_control_block *proc = pick_next_task_rt(rq)
    }
    // RR调度策略需要考虑时间片
    else if (current_pcb->policy == SCHED_RR)
    {
    }
    sti();
}