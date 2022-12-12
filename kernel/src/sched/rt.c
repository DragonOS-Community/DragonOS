#include "rt.h"

struct rt_rq *rt_rq_tmp;
extern struct rq rq_tmp;
/**
 * @brief 初始化RT进程调度器
 *
 */
void sched_rt_init(struct rt_rq *rt_rq)
{
    init_rt_rq(rt_rq);
}
void init_rt_rq(struct rt_rq *rt_rq)
{
    // 这里要不要分配内存，分配完能否在返回时正确传递？
    // rt_rq = (struct rt_rq *)kmalloc(sizeof(struct rt_rq), 0);
    for (int i = 0; i < MAX_RT_PRIO; i++)
    {
        list_init(&rt_rq->active.queue[i]);
    }
    rt_rq->rt_queued = 0;
    rt_rq->rt_time = 0;
    rt_rq->rt_runtime = 0;
}
static struct sched_rt_entity *pick_next_rt_entity(struct rt_rq *rt_rq)
{
    struct rt_prio_array *array = &rt_rq->active;
    struct sched_rt_entity *next = NULL;
    struct List *queue = NULL;
    int idx;

    // 此处查找链表中中下一个执行的entity
    // TODO :使用bitmap查找
    // idx = sched_find_first_bit(array->bitmap);
    for (int i = 0; i < MAX_CPU_NUM; i++)
    {
        if (!list_empty(&array->queue[i]))
        {
            // kdebug("priority=%d", i);
            queue = &array->queue[i];
            break;
        }
    }
    if (queue == NULL)
    {
        // kinfo("queue is null");
        return NULL;
    }
    // 获取当前的entry
    // next = list_entry(queue->next, struct sched_rt_entity, run_list);
    next = list_entry(list_next(queue), struct sched_rt_entity, run_list);
    // 获取后将该list移除出队列
    list_del(list_next(queue));
    // kinfo("get next is %p", next);

    return next;
}
static struct process_control_block *_pick_next_task_rt(struct rq *rq)
{
    struct sched_rt_entity *rt_se;
    struct rt_rq *rt_rq = &rq->rt_rq;
    // 从rt_rq中找优先级最高且最先入队的task
    rt_se = pick_next_rt_entity(rt_rq);
    // 队列中无法找到，返回空
    if (rt_se == NULL)
    {
        return NULL;
    }
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
    // TODO:这里同名会不会有影响？
    return container_of(rt_se, struct process_control_block, rt_se);
}

static void __enqueue_rt_entity(struct sched_rt_entity *rt_se, unsigned int flags)
{
    struct rt_prio_array *array = &rq_tmp.rt_rq.active;
    struct List *queue = &array->queue[rt_task_of(rt_se)->priority];
    list_append(queue, &rt_se->run_list);
    rt_se->on_list = 1;
    rt_se->on_rq = 1;
}

static void enqueue_rt_entity(struct sched_rt_entity *rt_se, unsigned int flags)
{
    __enqueue_rt_entity(rt_se, flags); // 将当前task enqueue到rt的rq中
}

/**
 * @brief 将rt_se插入到进程优先级对应的链表中
 *
 * @param rq
 * @param p
 * @param flags
 */
void enqueue_task_rt(struct rq *rq, struct process_control_block *p, int flags)
{
    struct sched_rt_entity *rt_se = &p->rt_se;
    enqueue_rt_entity(rt_se, flags);
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
}
// 目前没用到，考虑移除
static void dequeue_task_rt(struct rq *rq, struct process_control_block *p, int flags)
{
    struct sched_rt_entity *rt_se = &p->rt_se;
    dequeue_rt_entity(rt_se, flags);
}

/**
 * @brief 调度函数
 *
 */
void sched_rt()
{
    cli();
    bool need_change = false;
    struct process_control_block *proc = pick_next_task_rt(&rq_tmp);
    // 如果是fifo策略，则可以一直占有cpu直到有优先级更高的任务就绪(即使优先级相同也不行)或者主动放弃(等待资源)
    if (proc->policy == SCHED_FIFO)
    {
        // kinfo("begin sched_rt fifo");
        // 如果挑选的进程优先级小于当前进程，则不进行切换
        if (proc->priority <= current_pcb->priority)
        {
            enqueue_task_rt(&rq_tmp, proc, 0);
        }
        else
        {
            need_change = true;
        }
    }
    // RR调度策略需要考虑时间片
    else if (proc->policy == SCHED_RR)
    {
        // kinfo("begin sched_rt RR");
        // kinfo("sched_rt:current_pcb->priority %d", current_pcb->priority);
        // kinfo("sched_rt:proc->priority %d", proc->priority);
        // kinfo("sched_rt:proc->rt_se.time_slice %d", proc->rt_se.time_slice);
        if (proc->priority > current_pcb->priority)
        {
            // 判断这个进程时间片是否耗尽，若耗尽则将其时间片赋初值然后入队
            if (proc->rt_se.time_slice-- <= 0)
            {
                proc->rt_se.time_slice = RR_TIMESLICE;
                proc->flags |= PF_NEED_SCHED;
                enqueue_task_rt(&rq_tmp, proc, 0);
                // kinfo("sched_rt:if after rt proc->rt_se.time_slice %d", proc->rt_se.time_slice);

            }
            // 目标进程时间片未耗尽，切换到目标进程
            else
            {
                // kinfo("sched_rt:else after rt proc->rt_se.time_slice %d", proc->rt_se.time_slice);
                need_change = true;
            }

        }
        // curr优先级更大，说明一定是实时进程，则减去消耗时间片
        else
        {
            current_pcb->rt_se.time_slice--;
            enqueue_task_rt(&rq_tmp, proc, 0);
        }
    }
    kinfo("sched_rt:change to proc->pid %d", proc->pid);
    // kinfo("sched_rt:after rt proc->rt_se.time_slice %d", proc->rt_se.time_slice);
    if(need_change){
        process_switch_mm(proc);
        switch_proc(current_pcb, proc);
    }

    sti();
}