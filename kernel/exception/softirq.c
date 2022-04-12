#include "softirq.h"
#include <common/kprint.h>

void set_softirq_status(uint64_t status)
{
    softirq_status |= status;
}

uint64_t get_softirq_status()
{
    return softirq_status;
}

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
    for (uint32_t i = 0; i < MAX_SOFTIRQ_NUM && softirq_status; ++i)
    {
        if (softirq_status & (1 << i))
        {
            softirq_vector[i].action(softirq_vector[i].data);
            softirq_status &= (~(1 << i));
        }
    }

    
}

void softirq_init()
{
    softirq_status = 0;
    memset(softirq_vector, 0, sizeof(struct softirq_t) * MAX_SOFTIRQ_NUM);
}
