#include "softirq.h"
#include <common/kprint.h>
#include <process/process.h>
#include <driver/video/video.h>
#include <common/spinlock.h>

static spinlock_t softirq_modify_lock; // 软中断状态（status）
static volatile uint64_t softirq_pending = 0;
static volatile uint64_t softirq_running = 0;

void set_softirq_pending(uint64_t status)
{
    softirq_pending |= status;
}

uint64_t get_softirq_pending()
{
    return softirq_pending;
}

#define get_softirq_running() (softirq_running)

/**
 * @brief 设置软中断运行结束
 *
 * @param softirq_num
 */
#define clear_softirq_running(softirq_num)        \
    do                                            \
    {                                             \
        softirq_running &= (~(1 << softirq_num)); \
    } while (0)

// 设置软中断的运行状态（只应在do_softirq中调用此宏）
#define set_softirq_running(softirq_num)       \
    do                                         \
    {                                          \
        softirq_running |= (1 << softirq_num); \
    } while (0)

/**
 * @brief 清除软中断pending标志位
 *
 */
#define softirq_ack(sirq_num)                  \
    do                                         \
    {                                          \
        softirq_pending &= (~(1 << sirq_num)); \
    } while (0);

/**
 * @brief 软中断注册函数
 *
 * @param irq_num 软中断号
 * @param action 响应函数
 * @param data 响应数据结构体
 */
void register_softirq(uint32_t irq_num, void (*action)(void *data), void *data)
{
    softirq_vector[irq_num].action = action;
    softirq_vector[irq_num].data = data;
}

/**
 * @brief 卸载软中断
 *
 * @param irq_num 软中断号
 */
void unregister_softirq(uint32_t irq_num)
{
    softirq_vector[irq_num].action = NULL;
    softirq_vector[irq_num].data = NULL;
}

/**
 * @brief 软中断处理程序
 *
 */
void do_softirq()
{
    sti();

    for (uint32_t i = 0; i < MAX_SOFTIRQ_NUM && softirq_pending; ++i)
    {
        if (softirq_pending & (1 << i) && softirq_vector[i].action != NULL && (!(get_softirq_running() & (1 << i))))
        {
            if (spin_trylock(&softirq_modify_lock))
            {
                // 检测该软中断是否已经被其他进程执行
                if(get_softirq_running() & (1 << i))
                {
                    spin_unlock(&softirq_modify_lock);
                    continue;
                }
                softirq_ack(i);
                set_softirq_running(i);
                spin_unlock(&softirq_modify_lock);

                softirq_vector[i].action(softirq_vector[i].data);

                clear_softirq_running(i);
            }
        }
    }

    cli();
}

int clear_softirq_pending(uint32_t irq_num)
{
    clear_softirq_running(irq_num);
}

void softirq_init()
{
    softirq_pending = 0;
    memset(softirq_vector, 0, sizeof(struct softirq_t) * MAX_SOFTIRQ_NUM);
    spin_init(&softirq_modify_lock);
}
