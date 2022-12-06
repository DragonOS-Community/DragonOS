#include "sched.h"
#include <common/kprint.h>
#include <common/spinlock.h>
#include <driver/video/video.h>
#include <sched/cfs.h>
#include "sched/rt.h"
#include <common/string.h>

struct rq rq_tmp;

struct rq get_rq()
{
    return rq_tmp;
}
/**
 * @brief
 *
 * @param p pcb
 * @param attr 调度属性
 * @param user 请求是否来自用户态
 * @param pi
 * @return int
 */
static int __sched_setscheduler(struct process_control_block *p, const struct sched_attr *attr, bool user, bool pi)
{
    int policy = attr->sched_policy;
recheck:;
    // 这里policy的设置小于0是因为，需要在临界区内更新值之后，重新到这里判断
    if (!IS_VALID_SCHED_POLICY(policy))
    {
        return -EINVAL;
    }
    // 修改成功
    p->policy = policy;
    return 0;
}

static int _sched_setscheduler(struct process_control_block *p, int policy, const struct sched_param *param, bool check)
{
    struct sched_attr attr = {.sched_policy = policy};

    return __sched_setscheduler(p, &attr, check, true);
}

/**
 * sched_setscheduler -设置进程的调度策略
 * @param p 需要修改的pcb
 * @param policy 需要设置的policy
 * @param param structure containing the new RT priority. 目前没有用
 *
 * @return 成功返回0,否则返回对应的错误码
 *
 */
int sched_setscheduler(struct process_control_block *p, int policy, const struct sched_param *param)
{
    return _sched_setscheduler(p, policy, param, true);
}

int sched_getscheduler(struct process_control_block *p, int policy, const struct sched_param *param)
{
    return 0;
}
/**
 * @brief 包裹shced_cfs_enqueue(),将PCB加入就绪队列
 *
 * @param pcb
 */
void sched_enqueue(struct process_control_block *pcb)
{
    if(pcb->policy == SCHED_RR)
    {
        kinfo("policy is %d", pcb->policy);
        // 把pcb初始化一下，因为还没有找到进程创建后如何初始化，所以暂时在这里做测试
        struct sched_rt_entity rt_se;
        struct rt_rq myrt_rq;
        struct rt_prio_array active2;
        for (int i = 0; i < MAX_RT_PRIO; i++)
        {
            list_init(active2.queue + i);
        }
        myrt_rq.active = active2;
        myrt_rq.rt_queued = 0;
        myrt_rq.rt_time = 0;
        myrt_rq.rt_runtime = 0;
        rt_se.rt_rq = &myrt_rq;
        pcb->rt = rt_se;
        list_init(&pcb->rt.run_list);
        pcb->priority=10;
        kinfo("create pid is %d",pcb->pid);
        kinfo("set sched_rt_entity is %p",pcb->rt);
        // 测试把pcb加入队列
        enqueue_task_rt(&rq_tmp, pcb, 1);
        // dequeue_task_rt(&rq_tmp, pcb, 1);
        kinfo("pick next task begin!");
        // 测试获取下一个进程
        struct process_control_block * pcb_res=pick_next_task_rt(&rq_tmp);
        
        kinfo("pick next pid is %d",pcb_res->pid);
        kinfo("pick next task end!");

    }
    else {
        sched_cfs_enqueue(pcb);
    }
}

/**
 * @brief 包裹sched_cfs(),调度函数
 *
 */
void sched()
{
    sched_cfs();
}

void sched_init()
{
    // memset(&rq_tmp,0,sizeof(struct rq));
    // memset(&rq_tmp.rt,0,sizeof(struct rt_rq));
    kinfo("sched_init!");
    memset(&rq_tmp, 0, sizeof(struct rq));
    sched_cfs_init();
    sched_rt_init(&(rq_tmp.rt));
}
