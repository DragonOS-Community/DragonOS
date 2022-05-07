#include <libc/stdlib.h>
#include <libsystem/syscall.h>
#include <libc/stddef.h>
#include <libc/unistd.h>
#include <libc/errno.h>
#include <libc/stdio.h>

/**
 * @brief 显式链表的结点
 *
 */
typedef struct malloc_mem_chunk_t
{
    uint64_t start_addr;             // 整个块所在内存区域的起始地址（包括header）
    uint64_t length;                 // 整个块所占用的内存区域的大小
    struct malloc_mem_chunk_t *prev; // 上一个结点的指针
    struct malloc_mem_chunk_t *next; // 下一个结点的指针
} malloc_mem_chunk_t;

static uint64_t brk_base_addr = 0;    // 堆区域的内存基地址
static uint64_t brk_max_addr = 0;     // 堆区域的内存最大地址
static uint64_t brk_managed_addr = 0; // 堆区域已经被管理的地址

// 空闲链表
//  按start_addr升序排序
static malloc_mem_chunk_t *malloc_free_list = NULL;
// 已分配链表
//  使用LIFO策略。基于假设：程序运行早期分配的内存会被最晚释放
static malloc_mem_chunk_t *malloc_allocated_list = NULL;

/**
 * @brief 获取一块堆内存（不尝试扩大堆内存）
 *
 * @param size
 * @return void* 内存的地址指针，获取失败时返回-ENOMEM
 */
static void *malloc_no_enlarge(ssize_t size);

/**
 * @brief 将块插入空闲链表
 *
 * @param ck 待插入的块
 */
static void malloc_insert_free_list(malloc_mem_chunk_t *ck);

/**
 * @brief 在链表中检索符合要求的空闲块（best fit）
 *
 * @param size 块的大小
 * @return malloc_mem_chunk_t*
 */
static malloc_mem_chunk_t *malloc_query_free_chunk_bf(uint64_t size)
{
    // 在满足best fit的前提下，尽可能的使分配的内存在低地址
    //  使得总的堆内存可以更快被释放

    if (malloc_free_list == NULL)
    {
        printf("free list is none.\n");
        return NULL;
    }
    malloc_mem_chunk_t *ptr = malloc_free_list;
    malloc_mem_chunk_t *best = NULL;
    printf("query size=%d", size);
    while (ptr)
    {
        printf("ptr->length=%#010lx\n", ptr->length);
        if (ptr->length == size)
        {
            best = ptr;
            break;
        }

        if (ptr->length > size)
        {
            if (best == NULL)
                best = ptr;
            else if (best->length > ptr->length)
                best = ptr;
        }
        ptr = ptr->next;
    }

    return best;
}

/**
 * @brief 在链表中检索符合要求的空闲块（first fit）
 *
 * @param size
 * @return malloc_mem_chunk_t*
 */
static malloc_mem_chunk_t *malloc_query_free_chunk_ff(uint64_t size)
{
    if (malloc_free_list == NULL)
        return NULL;
    malloc_mem_chunk_t *ptr = malloc_free_list;

    while (ptr)
    {
        if (ptr->length >= size)
        {
            return ptr;
        }
        ptr = ptr->next;
    }

    return NULL;
}

/**
 * @brief 扩容malloc管理的内存区域
 *
 * @param size 扩大的内存大小
 */
static int malloc_enlarge(int32_t size)
{
    if (brk_base_addr == 0) // 第一次调用，需要初始化
    {
        brk_base_addr = brk(-1);
        printf("brk_base_addr=%#018lx\n", brk_base_addr);
        brk_managed_addr = brk_base_addr;
        brk_max_addr = brk(-2);
    }

    int64_t tmp = brk_managed_addr + size - brk_max_addr;
    if (tmp > 0) // 现有堆空间不足
    {
        if (sbrk(tmp) != (-1))
            brk_max_addr = brk((-1));
        else
        {
            put_string("malloc_enlarge(): no_mem", COLOR_YELLOW, COLOR_BLACK);
            return -ENOMEM;
        }
    }

    // 扩展管理的堆空间
    // 在新分配的内存的底部放置header
    malloc_mem_chunk_t *new_ck = (malloc_mem_chunk_t *)brk_managed_addr;
    new_ck->start_addr = (uint64_t)new_ck;
    new_ck->length = brk_max_addr - brk_managed_addr;
    printf("new_ck->start_addr=%#018lx\tbrk_max_addr=%#018lx\tbrk_managed_addr=%#018lx\n", new_ck->start_addr, brk_max_addr, brk_managed_addr);
    new_ck->prev = new_ck->next = NULL;
    brk_managed_addr = brk_max_addr;

    malloc_insert_free_list(new_ck);

    return 0;
}

/**
 * @brief 合并空闲块
 *
 */
static void malloc_merge_free_chunk()
{
    if (malloc_free_list == NULL)
        return;
    malloc_mem_chunk_t *ptr = malloc_free_list->next;
    while (ptr)
    {
        // 内存块连续
        if (ptr->prev->start_addr + ptr->prev->length == ptr->start_addr)
        {
            // 将ptr与前面的空闲块合并
            ptr->prev->length += ptr->length;
            ptr->prev->next = ptr->next;
            // 由于内存组成结构的原因，不需要free掉header
            ptr = ptr->prev;
        }
        ptr = ptr->next;
    }
}

/**
 * @brief 将块插入空闲链表
 *
 * @param ck 待插入的块
 */
static void malloc_insert_free_list(malloc_mem_chunk_t *ck)
{
    if (malloc_free_list == NULL) // 空闲链表为空
    {
        malloc_free_list = ck;
        ck->prev = ck->next = NULL;
        return;
    }
    else
    {
        uint64_t ck_end = ck->start_addr + ck->length;
        malloc_mem_chunk_t *ptr = malloc_free_list;
        while (ptr)
        {
            if (ptr->start_addr < ck->start_addr)
            {
                if (ptr->next == NULL) // 当前是最后一个项
                {
                    ptr->next = ck;
                    ck->next = NULL;
                    ck->prev = ptr;
                    break;
                }
                else if (ptr->next->start_addr > ck->start_addr)
                {
                    ck->prev = ptr;
                    ck->next = ptr->next;
                    ck->prev->next = ck;
                    ck->next->prev = ck;
                    break;
                }
            }
            else // 在ptr之前插入
            {

                if (ptr->prev == NULL) // 是第一个项
                {
                    malloc_free_list = ck;
                    ck->prev = NULL;
                    ck->next = ptr;
                    ptr->prev = ck;
                    break;
                }
                else
                {
                    ck->prev = ptr->prev;
                    ck->next = ptr;
                    ck->prev->next = ck;
                    ptr->prev = ck;
                    break;
                }
            }
            ptr = ptr->next;
        }
    }
}

/**
 * @brief 获取一块堆内存（不尝试扩大堆内存）
 *
 * @param size
 * @return void* 内存的地址指针，获取失败时返回-ENOMEM
 */
static void *malloc_no_enlarge(ssize_t size)
{
    // 加上header的大小
    size += sizeof(malloc_mem_chunk_t);

    // 采用best fit
    malloc_mem_chunk_t *ck = malloc_query_free_chunk_bf(size);

    if (ck == NULL) // 没有空闲块
    {
        // 尝试合并空闲块

        malloc_merge_free_chunk();
        ck = malloc_query_free_chunk_bf(size);

        // 找到了合适的块
        if (ck)
            goto found;
        else
            return -ENOMEM; // 内存不足
    }
found:;
    // 分配空闲块
    // 从空闲链表取出
    if (ck->prev == NULL) // 当前是链表的第一个块
    {
        malloc_free_list = ck->next;
    }
    else
        ck->prev->next = ck->next;

    if (ck->next != NULL) // 当前不是最后一个块
        ck->next->prev = ck->prev;

    // 当前块剩余的空间还能容纳多一个结点的空间，则分裂当前块
    if (ck->length - size > sizeof(malloc_mem_chunk_t))
    {
        malloc_mem_chunk_t *new_ck = ((uint64_t)ck) + ck->length;
        new_ck->length = ck->length - size;
        new_ck->start_addr = (uint64_t)new_ck;
        new_ck->prev = new_ck->next = NULL;

        ck->length = size;
        malloc_insert_free_list(new_ck);
    }

    // 插入到已分配链表
    // 直接插入到链表头，符合LIFO
    ck->prev = NULL;
    if (malloc_allocated_list) // 已分配链表不为空
    {
        malloc_allocated_list->prev = ck;
        ck->next = malloc_allocated_list;
        malloc_allocated_list = ck;
    }
    else // 已分配链表为空
    {
        malloc_allocated_list = ck;
        ck->next = NULL;
    }
    return (void *)(ck->start_addr + sizeof(malloc_mem_chunk_t));
}
/**
 * @brief 获取一块堆内存
 *
 * @param size 内存大小
 * @return void* 内存空间的指针
 */
void *malloc(ssize_t size)
{
    // 加上header的大小
    size += sizeof(malloc_mem_chunk_t);

    // 采用best fit
    malloc_mem_chunk_t *ck = malloc_query_free_chunk_bf(size);

    if (ck == NULL) // 没有空闲块
    {

        // 尝试合并空闲块
        printf("merge\n");
        malloc_merge_free_chunk();
        ck = malloc_query_free_chunk_bf(size);

        // 找到了合适的块
        if (ck)
            goto found;
        // 找不到合适的块，扩容堆区域
        printf("enlarge\n");
        if (malloc_enlarge(size) == -ENOMEM)
            return -ENOMEM; // 内存不足
        // 扩容后再次尝试获取
        printf("query\n");
        ck = malloc_query_free_chunk_bf(size);
    }
found:;
    if (ck == NULL)
        return -ENOMEM;
    // 分配空闲块
    // 从空闲链表取出
    if (ck->prev == NULL) // 当前是链表的第一个块
    {
        malloc_free_list = ck->next;
    }
    else
        ck->prev->next = ck->next;

    if (ck->next != NULL) // 当前不是最后一个块
        ck->next->prev = ck->prev;

    // 当前块剩余的空间还能容纳多一个结点的空间，则分裂当前块
    if (ck->length - size > sizeof(malloc_mem_chunk_t))
    {
        malloc_mem_chunk_t *new_ck = ((uint64_t)ck) + ck->length;
        new_ck->length = ck->length - size;
        new_ck->start_addr = (uint64_t)new_ck;
        new_ck->prev = new_ck->next = NULL;

        ck->length = size;
        malloc_insert_free_list(new_ck);
    }

    // 插入到已分配链表
    // 直接插入到链表头，符合LIFO
    ck->prev = NULL;
    if (malloc_allocated_list) // 已分配链表不为空
    {
        malloc_allocated_list->prev = ck;
        ck->next = malloc_allocated_list;
        malloc_allocated_list = ck;
    }
    else // 已分配链表为空
    {
        malloc_allocated_list = ck;
        ck->next = NULL;
    }
    printf("ck=%lld\n", (uint64_t)ck);
    printf("ck->start_addr=%lld\n", ck->start_addr);
    return (void *)(ck->start_addr + sizeof(malloc_mem_chunk_t));
}

/**
 * @brief 释放一块堆内存
 *
 * @param ptr 堆内存的指针
 */
void free(void *ptr)
{
}
