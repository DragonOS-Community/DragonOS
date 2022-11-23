#include <common/kfifo.h>
#include <common/glib.h>
#include <common/errno.h>
#include <common/compiler.h>
#include <mm/slab.h>

/**
 * @brief 通过动态方式初始化kfifo缓冲队列
 *
 * @param fifo 队列结构体
 * @param size 缓冲区大小
 * @param reserved 暂时保留，请置为0
 * @return int 错误码：成功->0
 */
int kfifo_alloc(struct kfifo_t *fifo, uint32_t size, uint64_t reserved)
{
    memset(fifo, 0, sizeof(struct kfifo_t));
    fifo->buffer = kmalloc(size, 0);
    if (fifo->buffer == NULL)
        goto failed;

    fifo->total_size = size;
    return 0;
failed:;
    return -ENOMEM;
}

/**
 * @brief 使用指定的缓冲区来初始化kfifo缓冲队列
 *
 * @param fifo 队列结构体
 * @param buffer 缓冲区
 * @param size 缓冲区大小
 */
void kfifo_init(struct kfifo_t *fifo, void *buffer, uint32_t size)
{
    memset(fifo, 0, sizeof(struct kfifo_t));

    fifo->buffer = buffer;
    fifo->total_size = size;
}

/**
 * @brief 向kfifo缓冲区推入数据
 *
 * @param fifo 队列结构体
 * @param from 来源数据地址
 * @param size 数据大小（字节数）
 * @return uint32_t 推入的数据大小
 */
uint32_t kfifo_in(struct kfifo_t *fifo, const void *from, uint32_t size)
{
    // 判断空间是否够
    if (unlikely(fifo->size + size > fifo->total_size))
        return 0;
    if (unlikely(from == NULL))
        return 0;

    // 分两种情况，一种是要发生回环，另一种不发生回环
    if (fifo->in_offset + size > fifo->total_size) // 发生回环
    {
        uint32_t tmp = fifo->total_size - fifo->in_offset;
        memcpy(fifo->buffer + fifo->in_offset, from, tmp);
        memcpy(fifo->buffer, from + tmp, size - tmp);
        fifo->in_offset = size - tmp;
    }
    else // 不发生回环
    {
        memcpy(fifo->buffer + fifo->in_offset, from, size);
        fifo->in_offset += size;
    }

    fifo->size += size;

    return size;
}

/**
 * @brief 从kfifo缓冲区取出数据，并从队列中删除数据
 *
 * @param fifo 队列结构体
 * @param to 拷贝目标地址
 * @param size 数据大小（字节数）
 * @return uint32_t 取出的数据大小
 */
uint32_t kfifo_out(struct kfifo_t *fifo, void *to, uint32_t size)
{
    if (unlikely(to == NULL)) // 判断目标地址是否为空
        return 0;
    if (unlikely(size > fifo->size)) // 判断队列中是否有这么多数据
        return 0;

    // 判断是否会发生回环
    if (fifo->out_offset + size > fifo->total_size) // 发生回环
    {
        uint32_t tmp = fifo->total_size - fifo->out_offset;
        memcpy(to, fifo->buffer + fifo->out_offset, tmp);
        memcpy(to + tmp, fifo->buffer, size - tmp);
        fifo->out_offset = size - tmp;
    }
    else // 未发生回环
    {
        memcpy(to, fifo->buffer + fifo->out_offset, size);
        fifo->out_offset += size;
    }

    fifo->size -= size;

    return size;
}

/**
 * @brief 从kfifo缓冲区取出数据，但是不从队列中删除数据
 *
 * @param fifo 队列结构体
 * @param to 拷贝目标地址
 * @param size 数据大小（字节数）
 * @return uint32_t 取出的数据大小
 */
uint32_t kfifo_out_peek(struct kfifo_t *fifo, void *to, uint32_t size)
{
    if (unlikely(to == NULL)) // 判断目标地址是否为空
        return 0;
    if (unlikely(size > fifo->size)) // 判断队列中是否有这么多数据
        return 0;

    // 判断是否会发生回环
    if (fifo->out_offset + size > fifo->total_size) // 发生回环
    {
        uint32_t tmp = fifo->total_size - fifo->out_offset;
        memcpy(to, fifo->buffer + fifo->out_offset, tmp);
        memcpy(to + tmp, fifo->buffer, size - tmp);
    }
    else // 未发生回环
    {
        memcpy(to, fifo->buffer + fifo->out_offset, size);
    }

    return size;
}

/**
 * @brief 释放通过kfifo_alloc创建的fifo缓冲区
 *
 * @param fifo fifo队列结构体
 */
void kfifo_free_alloc(struct kfifo_t *fifo)
{
    kfree(fifo->buffer);
    memset(fifo, 0, sizeof(struct kfifo_t));
}