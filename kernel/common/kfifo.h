#pragma once

#include <stdint.h>
#include <common/spinlock.h>

struct kfifo_t
{
    uint32_t total_size; // 缓冲区总空间
    uint32_t size;       // 元素所占的字节数
    uint32_t in_offset;  // 入口偏移
    uint32_t out_offset; // 出口偏移
    void *buffer;        // 缓冲区
} __attribute__((aligned(sizeof(long))));

/**
 * @brief 忽略kfifo队列中的所有内容，并把输入和输出偏移量都归零
 *
 */
#define kfifo_reset(fifo) (void)({ \
    (fifo)->size = 0;              \
    (fifo)->in_offset = 0;         \
    (fifo)->out_offset = 0;        \
})

/**
 * @brief 忽略kfifo队列中的所有内容，并将输入偏移量赋值给输出偏移量
 *
 */
#define kfifo_reset_out(fifo) (void)({      \
    (fifo)->size = 0;                       \
    (fifo)->out_offset = (fifo)->in_offset; \
})

/**
 * @brief 获取kfifo缓冲区的最大大小
 *
 * @param fifo 队列结构体
 * @return uint32_t 缓冲区最大大小
 */
#define kfifo_total_size(fifo) ((fifo)->total_size)
/**
 * @brief 获取kfifo缓冲区当前已使用的大小
 *
 * @param fifo 队列结构体
 * @return uint32_t 缓冲区当前已使用的大小
 */
#define kfifo_size(fifo) ((fifo)->size)

/**
 * @brief 判断kfifo缓冲区当前是否为空
 *
 * @param fifo 队列结构体
 * @return uint32_t 0->非空， 1->空
 */
#define kfifo_empty(fifo) (((fifo)->size == 0) ? 1 : 0)

/**
 * @brief 判断kfifo缓冲区当前是否为满
 *
 * @param fifo 队列结构体
 * @return uint32_t 0->不满， 1->满
 */
#define kfifo_full(fifo) (((fifo)->size == (fifo)->total_size) ? 1 : 0)

/**
 * @brief 通过动态方式初始化kfifo缓冲队列
 *
 * @param fifo 队列结构体
 * @param size 缓冲区大小
 * @param reserved 暂时保留，请置为0
 * @return int 错误码：成功->0
 */
int kfifo_alloc(struct kfifo_t *fifo, uint32_t size, uint64_t reserved);

/**
 * @brief 释放通过kfifo_alloc创建的fifo缓冲区
 *
 * @param fifo fifo队列结构体
 */
void kfifo_free_alloc(struct kfifo_t *fifo);

/**
 * @brief 使用指定的缓冲区来初始化kfifo缓冲队列
 *
 * @param fifo 队列结构体
 * @param buffer 缓冲区
 * @param size 缓冲区大小
 */
void kfifo_init(struct kfifo_t *fifo, void *buffer, uint32_t size);

/**
 * @brief 向kfifo缓冲区推入指定大小的数据
 *
 * @param fifo 队列结构体
 * @param from 来源数据地址
 * @param size 数据大小（字节数）
 * @return uint32_t 推入的数据大小
 */
uint32_t kfifo_in(struct kfifo_t *fifo, const void *from, uint32_t size);

/**
 * @brief 从kfifo缓冲区取出数据，并从队列中删除数据
 *
 * @param fifo 队列结构体
 * @param to 拷贝目标地址
 * @param size 数据大小（字节数）
 * @return uint32_t 取出的数据大小
 */
uint32_t kfifo_out(struct kfifo_t *fifo, void *to, uint32_t size);

/**
 * @brief 从kfifo缓冲区取出数据，但是不从队列中删除数据
 *
 * @param fifo 队列结构体
 * @param to 拷贝目标地址
 * @param size 数据大小（字节数）
 * @return uint32_t 取出的数据大小
 */
uint32_t kfifo_out_peek(struct kfifo_t *fifo, void *to, uint32_t size);

/**
 * @brief 向kfifo缓冲区推入指定大小的数据并在过程加锁
 *
 * @param fifo 队列结构体
 * @param from 来源数据地址
 * @param size 数据大小（字节数）
 * @param lock 自旋锁
 * @return uint32_t 推入的数据大小
 */
uint32_t __always_inline kfifo_in_locked(struct kfifo_t *fifo, const void *from, uint32_t size, spinlock_t *lock)
{
    spin_lock(lock);
    uint32_t retval = kfifo_in(fifo, from, size);
    spin_unlock(lock);
    return retval;
}

/**
 * @brief 从kfifo缓冲区取出数据，并从队列中删除数据，并在过程加锁
 *
 * @param fifo 队列结构体
 * @param to 拷贝目标地址
 * @param size 数据大小（字节数）
 * @param lock 自旋锁
 * @return uint32_t 取出的数据大小
 */
uint32_t __always_inline kfifo_out_locked(struct kfifo_t *fifo, void *to, uint32_t size, spinlock_t *lock)
{
    spin_lock(lock);
    uint32_t retval = kfifo_out(fifo, to, size);
    spin_unlock(lock);
    return retval;
}
