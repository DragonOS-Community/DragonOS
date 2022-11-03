#include "common/completion.h"
#include "common/kthread.h"

/**
 * @brief 初始化一个completion变量
 *
 * @param x completion
 */
void completion_init(struct completion *x)
{
    x->done = 0;
    wait_queue_head_init(&x->wait_queue);
}

/**
 * @brief 唤醒一个wait_queue中的节点
 *
 * @param x completion
 */
void complete(struct completion *x)
{

    spin_lock(&x->wait_queue.lock);

    if (x->done != COMPLETE_ALL)
        ++(x->done);
    wait_queue_wakeup_on_stack(&x->wait_queue, -1UL); // -1UL代表所有节点都满足条件,暂时这么写

    spin_unlock(&x->wait_queue.lock);
}

/**
 * @brief 永久标记done为Complete_All, 并从wait_queue中删除所有节点
 *
 * @param x completion
 */
void complete_all(struct completion *x)
{
    spin_lock(&x->wait_queue.lock);

    x->done = COMPLETE_ALL; // 永久赋值
    while (!list_empty(&x->wait_queue.wait_list))
        wait_queue_wakeup_on_stack(&x->wait_queue, -1UL); // -1UL代表所有节点都满足条件,暂时这么写

    spin_unlock(&x->wait_queue.lock);
}

/**
 * @brief 辅助函数：通用的处理wait命令的函数(即所有wait_for_completion函数最核心部分在这里)
 *
 * @param x completion
 * @param action 函数指针
 * @param timeout 一个非负整数
 * @param state 你要设置进程的状态为state
 * @return long - 返回剩余的timeout
 */
static long __wait_for_common(struct completion *x, long (*action)(long), long timeout, int state)
{
    if (!x->done)
    {
        DECLARE_WAIT_ON_STACK_SELF(wait);

        while (!x->done && timeout > 0)
        {
            // 加入等待队列, 但是不会调度走
            if (list_empty(&wait.wait_list))
                list_append(&x->wait_queue.wait_list, &wait.wait_list);
            wait.pcb->state = state; // 清除运行位, 并设置为interuptible/uninteruptible

            spin_unlock(&x->wait_queue.lock);

            timeout = action(timeout);
            spin_lock(&x->wait_queue.lock);
        }
        if (!x->done)
            return timeout; // 仍然没有complete, 但是被其他进程唤醒

        wait.pcb->state = PROC_RUNNING; // 设置为运行, 并清空state， 所以使用等号赋值
        if (!list_empty(&wait.wait_list))
            list_del_init(&wait.wait_list); // 必须使用del_init
    }
    if (x->done != COMPLETE_ALL)
        --(x->done);
    return timeout ? timeout : 1; // 这里linux返回1，不知道为啥
}

/**
 * @brief 等待completion命令唤醒进程, 同时设置pcb->state为uninteruptible.
 *
 * @param x completion
 */
void wait_for_completion(struct completion *x)
{
    spin_lock(&x->wait_queue.lock);
    __wait_for_common(x, &schedule_timeout_ms, MAX_TIMEOUT, PROC_UNINTERRUPTIBLE);
    spin_unlock(&x->wait_queue.lock);
}

/**
 * @brief 等待指定时间，超时后就返回, 同时设置pcb->state为uninteruptible.
 *
 * @param x completion
 * @param timeout 非负整数，等待指定时间，超时后就返回/ 或者提前done，则返回剩余timeout时间
 * @return long - 返回剩余的timeout
 */
long wait_for_completion_timeout(struct completion *x, long timeout)
{
    BUG_ON(timeout < 0);
    spin_lock(&x->wait_queue.lock);
    timeout = __wait_for_common(x, &schedule_timeout_ms, timeout, PROC_UNINTERRUPTIBLE);
    spin_unlock(&x->wait_queue.lock);
    return timeout;
}

/**
 * @brief 等待completion的完成，但是可以被中断（我也不太懂可以被中断是什么意思，就是pcb->state=interuptible）
 *
 * @param x completion
 */
void wait_for_completion_interruptible(struct completion *x)
{
    spin_lock(&x->wait_queue.lock);
    __wait_for_common(x, &schedule_timeout_ms, MAX_TIMEOUT, PROC_INTERRUPTIBLE);
    spin_unlock(&x->wait_queue.lock);
}

/**
 * @brief 等待指定时间，超时后就返回, 等待completion的完成，但是可以被中断.
 *
 * @param x completion
 * @param timeout 非负整数，等待指定时间，超时后就返回/ 或者提前done，则返回剩余timeout时间
 * @return long - 返回剩余的timeout
 */
long wait_for_completion_interruptible_timeout(struct completion *x, long timeout)
{
    BUG_ON(timeout < 0);

    spin_lock(&x->wait_queue.lock);
    timeout = __wait_for_common(x, &schedule_timeout_ms, timeout, PROC_INTERRUPTIBLE);
    spin_unlock(&x->wait_queue.lock);
    return timeout;
}

/**
 * @brief 尝试获取completion的一个done！如果您在wait之前加上这个函数作为判断，说不定会加快运行速度。
 *
 * @param x completion
 * @return true - 表示不需要wait_for_completion，并且已经获取到了一个completion(即返回true意味着done已经被 减1 ) \
 * @return false - 表示当前done=0，您需要进入等待，即wait_for_completion
 */
bool try_wait_for_completion(struct completion *x)
{
    if (!READ_ONCE(x->done))
        return false;

    bool ret = true;
    spin_lock(&x->wait_queue.lock);

    if (!x->done)
        ret = false;
    else if (x->done != COMPLETE_ALL)
        --(x->done);

    spin_unlock(&x->wait_queue.lock);
    return ret;
}

/**
 * @brief 测试一个completion是否有waiter。(即done是不是等于0)
 *
 * @param x completion
 * @return true
 * @return false
 */
bool completion_done(struct completion *x)
{

    if (!READ_ONCE(x->done))
        return false;

    // 这里的意义是: 如果是多线程的情况下，您有可能需要等待另一个进程的complete操作, 才算真正意义上的completed!
    spin_lock(&x->wait_queue.lock);

    if (!READ_ONCE(x->done))
    {
        spin_unlock(&x->wait_queue.lock);
        return false;
    }
    spin_unlock(&x->wait_queue.lock);
    return true;
}

/**
 * @brief 对completion数组进行wait操作
 *
 * @param x completion array
 * @param n len of the array
 */
void wait_for_multicompletion(struct completion x[], int n)
{
    for (int i = 0; i < n; i++) // 对每一个completion都等一遍
    {
        if (!completion_done(&x[i])) // 如果没有done，直接wait
        {
            wait_for_completion(&x[i]);
        }
        else if (!try_wait_for_completion(&x[i])) //上面测试过done>0，那么这里尝试去获取一个done，如果失败了，就继续wait
        {
            wait_for_completion(&x[i]);
        }
    }
}

/**
 * @brief 等待者, 等待wait_for_completion
 *
 * @param one_to_one
 * @param one_to_many
 * @param many_to_one
 */
int __test_completion_waiter(void *input_data)
{
    struct __test_data *data = (struct __test_data *)input_data;
    // kdebug("THE %d WAITER BEGIN", -data->id);
    // 测试一对多能不能实现等待 - 由外部统一放闸一起跑
    if (!try_wait_for_completion(data->one_to_many))
    {
        wait_for_completion(data->one_to_many);
    }

    // 测试一对一能不能实现等待
    if (!try_wait_for_completion(data->one_to_many))
    {
        wait_for_completion(data->one_to_many);
    }

    // 完成上面两个等待, 执行complete声明自己已经完成
    complete(data->many_to_one);
    // kdebug("THE %d WAITER SOLVED", -data->id);
    return true;
}

/**
 * @brief 执行者，执行complete
 *
 * @param one_to_one
 * @param one_to_many
 * @param many_to_one
 */
int __test_completion_worker(void *input_data)
{
    struct __test_data *data = (struct __test_data *)input_data;
    // kdebug("THE %d WORKER BEGIN", data->id);
    // 测试一对多能不能实现等待 - 由外部统一放闸一起跑
    if (!try_wait_for_completion(data->one_to_many))
    {
        wait_for_completion(data->one_to_many);
    }

    schedule_timeout_ms(50);
    // for(uint64_t i=0;i<1e7;++i)
    //     pause();
    complete(data->one_to_one);

    // 完成上面两个等待, 执行complete声明自己已经完成
    complete(data->many_to_one);
    // kdebug("THE %d WORKER SOLVED", data->id);
    return true;
}

/**
 * @brief 测试函数
 *
 */
void __test_completion()
{
    // kdebug("BEGIN COMPLETION TEST");
    const int N = 100;
    struct completion *one_to_one = kzalloc(sizeof(struct completion) * N, 0);
    struct completion *one_to_many = kzalloc(sizeof(struct completion), 0);
    struct completion *waiter_many_to_one = kzalloc(sizeof(struct completion) * N, 0);
    struct completion *worker_many_to_one = kzalloc(sizeof(struct completion) * N, 0);
    struct __test_data *waiter_data = kzalloc(sizeof(struct __test_data) * N, 0);
    struct __test_data *worker_data = kzalloc(sizeof(struct __test_data) * N, 0);

    completion_init(one_to_many);
    for (int i = 0; i < N; i++)
    {
        completion_init(&one_to_one[i]);
        completion_init(&waiter_many_to_one[i]);
        completion_init(&worker_many_to_one[i]);
    }

    for (int i = 0; i < N; i++)
    {
        waiter_data[i].id = -i; // waiter
        waiter_data[i].many_to_one = &waiter_many_to_one[i];
        waiter_data[i].one_to_one = &one_to_one[i];
        waiter_data[i].one_to_many = one_to_many;
        kthread_run(__test_completion_waiter, &waiter_data[i], "the %dth waiter", i);
    }

    for (int i = 0; i < N; i++)
    {
        worker_data[i].id = i; // worker
        worker_data[i].many_to_one = &worker_many_to_one[i];
        worker_data[i].one_to_one = &one_to_one[i];
        worker_data[i].one_to_many = one_to_many;
        kthread_run(__test_completion_worker, &worker_data[i], "the %dth worker", i);
    }

    complete_all(one_to_many);
    // kdebug("all of the waiters and workers begin running");

    // kdebug("BEGIN COUNTING");

    wait_for_multicompletion(waiter_many_to_one, N);
    wait_for_multicompletion(worker_many_to_one, N);
    // kdebug("all of the waiters and workers complete");

    kfree(one_to_one);
    kfree(one_to_many);
    kfree(waiter_many_to_one);
    kfree(worker_many_to_one);
    kfree(waiter_data);
    kfree(worker_data);
    // kdebug("completion test done.");
}