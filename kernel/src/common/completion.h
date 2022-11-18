#include <common/spinlock.h>
#include <common/wait_queue.h>
#include <process/process.h>
#include <time/sleep.h>
#include <time/timer.h>

// 永久地设置该completion已经被完成,不会再有进程等待
#define COMPLETE_ALL UINT32_MAX

struct completion
{
    unsigned int done;
    wait_queue_head_t wait_queue;
};

#define DECLARE_COMPLETION_ON_STACK(name) \
    struct completion name = {0};         \
    completion_init(&name);

/**
 * 对外函数声明
 */
void completion_init(struct completion *x);
void complete(struct completion *x);
void complete_all(struct completion *x);
void wait_for_completion(struct completion *x);
long wait_for_completion_timeout(struct completion *x, long timeout);
void wait_for_completion_interruptible(struct completion *x);
long wait_for_completion_interruptible_timeout(struct completion *x, long timeout);
void wait_for_multicompletion(struct completion x[], int n);
bool try_wait_for_completion(struct completion *x);
bool completion_done(struct completion *x);

/**
 * 测试函数声明 (测试代码辅助函数)
 */
struct __test_data
{
    int id;
    struct completion *one_to_one;
    struct completion *one_to_many;
    struct completion *many_to_one;
};

int __test_completion_waiter(void *data); // 等待者
int __test_completion_worker(void *data); // 执行者
void __test_completion();