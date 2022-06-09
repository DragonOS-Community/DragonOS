#pragma once

#include <process/process.h>

/**
 * @brief 增加自旋锁计数变量
 * 
 */
#define preempt_disable()   \
do  \
{   \
    ++(current_pcb->preempt_count);\
} while (0)

/**
 * @brief 减少自旋锁计数变量
 * 
 */
#define preempt_enable()   \
do  \
{   \
    --(current_pcb->preempt_count);\
}while(0)
