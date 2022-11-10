#include <common/spinlock.h>
#include <common/wait_queue.h>

typedef struct
{
    struct List wait_list;
    spinlock_t lock; // 队列需要有一个自旋锁,虽然目前内部并没有使用,但是以后可能会用.[在completion内部使用]
} wait_queue_head_t;

#define DECLARE_WAIT_ON_STACK(name, pcb) \
    wait_queue_node_t name = {0};        \
    wait_queue_init(&(name), pcb);

#define DECLARE_WAIT_ON_STACK_SELF(name) \
    wait_queue_node_t name = {0};        \
    wait_queue_init(&(name), current_pcb);

#define DECLARE_WAIT_ALLOC(name, pcb)                                                     \
    wait_queue_node_t *wait = (wait_queue_node_t *)kzalloc(sizeof(wait_queue_node_t), 0); \
    wait_queue_init(&(name), pcb);

#define DECLARE_WAIT_ALLOC_SELF(name)                                                     \
    wait_queue_node_t *wait = (wait_queue_node_t *)kzalloc(sizeof(wait_queue_node_t), 0); \
    wait_queue_init(&(name), current_pcb);

#define DECLARE_WAIT_QUEUE_HEAD(name)    \
    struct wait_queue_head_t name = {0}; \
    wait_queue_head_init(&name);

/**
 * @brief 初始化wait_queue队列头
 *
 * @param wait_queue
 */
void wait_queue_head_init(wait_queue_head_t *wait_queue);

/**
 * @brief 在等待队列上进行等待, 但是你需要确保wait已经被init, 同时wakeup只能使用wake_up_on_stack函数。
 *
 * @param q 队列头指针
 * @param wait wait节点
 */
void wait_queue_sleep_with_node(wait_queue_head_t *q, wait_queue_node_t *wait);

/**
 * @brief  在等待队列上进行等待,同时释放自旋锁, 但是你需要确保wait已经被init, 同时wakeup只能使用wake_up_on_stack函数。
 *
 * @param q  队列头指针
 * @param wait wait节点
 * @param lock
 */
void wait_queue_sleep_with_node_unlock(wait_queue_head_t *q, wait_queue_node_t *wait, void *lock);

/**
 * @brief 在等待队列上进行等待(允许中断), 但是你需要确保wait已经被init, 同时wakeup只能使用wake_up_on_stack函数。
 *
 * @param wait_queue_head 队列头指针
 * @param wait wait节点
 */
void wait_queue_sleep_with_node_interriptible(wait_queue_head_t *q, wait_queue_node_t *wait);

/**
 * @brief 唤醒在等待队列的头部的进程, 但是不会free掉这个节点的空间(默认这个节点在栈上创建)
 *
 * @param wait_queue_head_t  q: 队列头
 * @param state 要唤醒的进程的状态
 */
void wait_queue_wakeup_on_stack(wait_queue_head_t *q, int64_t state);