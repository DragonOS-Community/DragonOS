#include "sched.h"
#include <common/kprint.h>
#include <common/spinlock.h>
#include <driver/video/video.h>
#include <sched/cfs.h>
#include <common/string.h>
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

/**
 * @brief 包裹shced_cfs_enqueue(),将PCB加入就绪队列
 *
 * @param pcb
 */
void sched_enqueue(struct process_control_block *pcb)
{
    sched_cfs_enqueue(pcb);
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
    sched_cfs_init();
}



