#include <common/glib.h>
#include <common/kthread.h>
#include <common/spinlock.h>
#include <debug/bug.h>
#include <sched/sched.h>
#include <time/sleep.h>

static spinlock_t __kthread_create_lock;           // kthread创建过程的锁
static struct List kthread_create_list;            // kthread创建任务的链表
struct process_control_block *kthreadd_pcb = NULL; // kthreadd守护线程的pcb

// 枚举各个标志位是在第几位
enum KTHREAD_BITS
{
    KTHREAD_IS_PER_CPU = 0,
    KTHREAD_SHOULD_STOP,
    KTHREAD_SHOULD_PARK,
};

/**
 * @brief kthread的创建信息（仅在创建过程中存在）
 *
 */
struct kthread_create_info_t
{
    // 传递给kthread的信息
    int (*thread_fn)(void *data);
    void *data;
    int node;

    // kthreadd守护进程传递给kthread_create的结果,
    // 成功则返回PCB，不成功则该值为负数错误码。若该值为NULL，意味着创建过程尚未完成
    struct process_control_block *result;

    struct List list;
};

/**
 * @brief 获取pcb中的kthread结构体
 *
 * @param pcb pcb
 * @return struct kthread* kthread信息结构体
 */
struct kthread_info_t *to_kthread(struct process_control_block *pcb)
{
    WARN_ON(!(pcb->flags & PF_KTHREAD));
    return pcb->worker_private;
}

static struct process_control_block *__kthread_create_on_node(int (*thread_fn)(void *data), void *data, int node,
                                                              const char name_fmt[], va_list args)
{
    struct process_control_block *pcb = NULL;
    struct kthread_create_info_t *create = kzalloc(sizeof(struct kthread_create_info_t), 0);

    if (create == NULL)
        return ERR_PTR(-ENOMEM);
    BUG_ON(name_fmt == NULL);

    create->thread_fn = thread_fn;
    create->data = data;
    create->node = node;
    create->result = NULL;
    list_init(&create->list);

    spin_lock(&__kthread_create_lock);
    list_append(&kthread_create_list, &create->list);
    spin_unlock(&__kthread_create_lock);
    // kdebug("to wakeup kthread daemon..., current preempt=%d, rflags=%#018lx", current_pcb->preempt_count,

    // todo: 使用completion优化这里
    while (kthreadd_pcb == NULL) // 若kthreadd未初始化，则等待kthreadd启动
        ;
    // 唤醒kthreadd守护进程
    process_wakeup_immediately(kthreadd_pcb);

    // 等待创建完成
    // todo: 使用completion机制以降低忙等时间
    while (create->result == NULL)
        pause();
    // 获取结果
    pcb = create->result;
    if (!IS_ERR(create->result))
    {
        // 为内核线程设置名字
        char pcb_name[PCB_NAME_LEN];
        va_list get_args;
        va_copy(get_args, args);
        // 获取到字符串的前16字节
        int len = vsnprintf(pcb_name, name_fmt, PCB_NAME_LEN, get_args);
        if (len >= PCB_NAME_LEN)
        {
            //名字过大 放到full_name字段中
            struct kthread_info_t *kthread = to_kthread(pcb);
            char *full_name = kzalloc(1024, 0);
            vsprintf(full_name, name_fmt, get_args);
            kthread->full_name = full_name;
        }
        // 将前16Bytes放到pcb的name字段
        process_set_pcb_name(pcb, pcb_name);
        va_end(get_args);
    }

    kfree(create);
    return pcb;
}

/**
 * @brief 让当前内核线程退出，并返回result参数给kthread_stop()函数
 *
 * @param result 返回值
 */
void kthread_exit(long result)
{
    struct kthread_info_t *kt = to_kthread(current_pcb);
    kt->result = result;
    kt->exited = true;
    process_do_exit(0);
}

/**
 * @brief 在当前结点上创建一个内核线程
 *
 * @param thread_fn 该内核线程要执行的函数
 * @param data 传递给 thread_fn 的参数数据
 * @param node 线程的任务和线程结构都分配在这个节点上
 * @param name_fmt printf-style format string for the thread name
 * @param arg name_fmt的参数
 * @return 返回一个pcb或者是ERR_PTR(-ENOMEM)
 *
 * 请注意，该宏会创建一个内核线程，并将其设置为停止状态。您可以使用wake_up_process来启动这个线程。
 * 新的线程的调度策略为SCHED_NORMAL，并且能在所有的cpu上运行
 *
 * 当内核线程被唤醒时，会运行thread_fn函数，并将data作为参数传入。
 * 内核线程可以直接返回，也可以在kthread_should_stop为真时返回。
 */
struct process_control_block *kthread_create_on_node(int (*thread_fn)(void *data), void *data, int node,
                                                     const char name_fmt[], ...)
{
    struct process_control_block *pcb;
    va_list args;
    va_start(args, name_fmt);
    pcb = __kthread_create_on_node(thread_fn, data, node, name_fmt, args);
    va_end(args);
    return pcb;
}
/**
 * @brief 内核线程的包裹程序
 * 当内核线程被运行后，从kernel_thread_func跳转到这里。
 * @param _create 内核线程的创建信息
 * @return int 内核线程的退出返回值
 */
static int kthread(void *_create)
{
    struct kthread_create_info_t *create = _create;
    // 将这几个信息从kthread_create_info中拷贝过来。以免在kthread_create_info被free后，数据丢失从而导致错误。
    int (*thread_fn)(void *data) = create->thread_fn;
    void *data = create->data;

    int retval = 0;

    struct kthread_info_t *self = to_kthread(current_pcb);

    self->thread_fn = thread_fn;
    self->data = data;

    // todo: 增加调度参数设定
    // todo: 当前内核线程继承了kthreadd的优先级以及调度策略，需要在这里进行更新

    // 设置当前进程为不可被打断
    current_pcb->state = PROC_UNINTERRUPTIBLE;

    // 将当前pcb返回给创建者
    create->result = current_pcb;

    current_pcb->state &= ~PROC_RUNNING;    // 设置当前进程不是RUNNING态
    io_mfence();

    // 发起调度，使得当前内核线程休眠。直到创建者通过process_wakeup将当前内核线程唤醒
    sched();

    retval = -EINTR;
    // 如果发起者没有调用kthread_stop()，则该kthread的功能函数开始执行
    if (!(self->flags & (1 << KTHREAD_SHOULD_STOP)))
    {
        retval = thread_fn(data);
    }
    kthread_exit(retval);
}

static void __create_kthread(struct kthread_create_info_t *create)
{
    pid_t pid = kernel_thread(kthread, create, CLONE_FS | CLONE_SIGNAL);
    io_mfence();
    if (IS_ERR((void *)pid))
    {
        // todo: 使用complete机制完善这里

        create->result = (struct process_control_block *)pid;
    }
}

/**
 * @brief kthread守护线程
 *
 * @param unused
 * @return int 不应当退出
 */
int kthreadd(void *unused)
{
    kinfo("kthread daemon started!");
    struct process_control_block *pcb = current_pcb;
    kthreadd_pcb = current_pcb;
    current_pcb->flags |= PF_NOFREEZE;

    for (;;)
    {
        current_pcb->state = PROC_INTERRUPTIBLE;
        // 所有的创建任务都被处理完了
        if (list_empty(&kthread_create_list))
            sched();

        spin_lock(&__kthread_create_lock);
        // 循环取出链表中的任务
        while (!list_empty(&kthread_create_list))
        {

            // 从链表中取出第一个要创建的内核线程任务
            struct kthread_create_info_t *create =
                container_of(kthread_create_list.next, struct kthread_create_info_t, list);
            list_del_init(&create->list);
            spin_unlock(&__kthread_create_lock);

            __create_kthread(create);

            spin_lock(&__kthread_create_lock);
        }
        spin_unlock(&__kthread_create_lock);
    }
}

/**
 * @brief 内核线程调用该函数，检查自身的标志位，判断自己是否应该执行完任务后退出
 *
 * @return true 内核线程应该退出
 * @return false 无需退出
 */
bool kthread_should_stop(void)
{
    struct kthread_info_t *self = to_kthread(current_pcb);
    if (self->flags & (1 << KTHREAD_SHOULD_STOP))
        return true;

    return false;
}

/**
 * @brief 向kthread发送停止信号，请求其结束
 *
 * @param pcb 内核线程的pcb
 * @return int 错误码
 */
int kthread_stop(struct process_control_block *pcb)
{
    int retval;
    struct kthread_info_t *target = to_kthread(pcb);
    target->flags |= (1 << KTHREAD_SHOULD_STOP);
    process_wakeup(pcb);
    // 等待指定的内核线程退出
    // todo: 使用completion机制改进这里
    while (target->exited == false)
        usleep(5000);
    retval = target->result;

    // 释放内核线程的页表
    process_exit_mm(pcb);
    process_release_pcb(pcb);
    return retval;
}

/**
 * @brief 设置pcb中的worker_private字段（只应被设置一次）
 *
 * @param pcb pcb
 * @return bool 成功或失败
 */
bool kthread_set_worker_private(struct process_control_block *pcb)
{
    if (WARN_ON_ONCE(to_kthread(pcb)))
        return false;

    struct kthread_info_t *kt = kzalloc(sizeof(struct kthread_info_t), 0);
    if (kt == NULL)
        return false;
    pcb->worker_private = kt;
    return true;
}

/**
 * @brief 初始化kthread机制(只应被process_init调用)
 *
 * @return int 错误码
 */
int kthread_mechanism_init()
{
    kinfo("Initializing kthread mechanism...");
    spin_init(&__kthread_create_lock);
    list_init(&kthread_create_list);
    // 创建kthreadd守护进程
    kernel_thread(kthreadd, NULL, CLONE_FS | CLONE_SIGNAL);

    return 0;
}

/**
 * @brief 释放pcb指向的worker private
 *
 * @param pcb 要释放的pcb
 */
void free_kthread_struct(struct process_control_block *pcb)
{
    struct kthread_info_t *kthread = to_kthread(pcb);
    if (!kthread)
    {
        return;
    }
    pcb->worker_private = NULL;
    kfree(kthread->full_name);
    kfree(kthread);
}