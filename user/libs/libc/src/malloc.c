#include <libc/src/stdlib.h>
#include <libsystem/syscall.h>
#include <libc/src/stddef.h>
#include <libc/src/unistd.h>
#include <libc/src/errno.h>
#include <libc/src/stdio.h>

#define PAGE_4K_SHIFT 12
#define PAGE_2M_SHIFT 21
#define PAGE_1G_SHIFT 30
#define PAGE_GDT_SHIFT 39

// 不同大小的页的容量
#define PAGE_4K_SIZE (1UL << PAGE_4K_SHIFT)
#define PAGE_2M_SIZE (1UL << PAGE_2M_SHIFT)
#define PAGE_1G_SIZE (1UL << PAGE_1G_SHIFT)

// 屏蔽低于x的数值
#define PAGE_4K_MASK (~(PAGE_4K_SIZE - 1))
#define PAGE_2M_MASK (~(PAGE_2M_SIZE - 1))

// 将addr按照x的上边界对齐
#define PAGE_4K_ALIGN(addr) (((unsigned long)(addr) + PAGE_4K_SIZE - 1) & PAGE_4K_MASK)
#define PAGE_2M_ALIGN(addr) (((unsigned long)(addr) + PAGE_2M_SIZE - 1) & PAGE_2M_MASK)

/**
 * @brief 显式链表的结点
 *
 */
typedef struct malloc_mem_chunk_t
{
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
static malloc_mem_chunk_t *malloc_free_list_end = NULL; // 空闲链表的末尾结点

static uint64_t count_last_free_size = 0; // 统计距离上一次回收内存，已经free了多少内存

/**
 * @brief 将块插入空闲链表
 *
 * @param ck 待插入的块
 */
static void malloc_insert_free_list(malloc_mem_chunk_t *ck);

/**
 * @brief 当堆顶空闲空间大于2个页的空间的时候，释放1个页
 *
 */
static void release_brk();

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
        return NULL;
    }
    malloc_mem_chunk_t *ptr = malloc_free_list;
    malloc_mem_chunk_t *best = NULL;
    // printf("query size=%d", size);
    while (ptr != NULL)
    {
        // printf("ptr->length=%#010lx\n", ptr->length);
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
static int malloc_enlarge(int64_t size)
{
    if (brk_base_addr == 0) // 第一次调用，需要初始化
    {
        brk_base_addr = brk(-1);
        // printf("brk_base_addr=%#018lx\n", brk_base_addr);
        brk_managed_addr = brk_base_addr;
        brk_max_addr = brk(-2);
    }

    int64_t free_space = brk_max_addr - brk_managed_addr;
    // printf("size=%ld\tfree_space=%ld\n", size, free_space);
    if (free_space < size) // 现有堆空间不足
    {
        if (sbrk(size - free_space) != (void *)(-1))
            brk_max_addr = brk((-2));
        else
        {
            put_string("malloc_enlarge(): no_mem\n", COLOR_YELLOW, COLOR_BLACK);
            return -ENOMEM;
        }

        // printf("brk max addr = %#018lx\n", brk_max_addr);
    }

    // 扩展管理的堆空间
    // 在新分配的内存的底部放置header
    // printf("managed addr = %#018lx\n", brk_managed_addr);
    malloc_mem_chunk_t *new_ck = (malloc_mem_chunk_t *)brk_managed_addr;
    new_ck->length = brk_max_addr - brk_managed_addr;
    // printf("new_ck->start_addr=%#018lx\tbrk_max_addr=%#018lx\tbrk_managed_addr=%#018lx\n", (uint64_t)new_ck, brk_max_addr, brk_managed_addr);
    new_ck->prev = NULL;
    new_ck->next = NULL;
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
    while (ptr != NULL)
    {
        // 内存块连续
        if (((uint64_t)(ptr->prev) + ptr->prev->length == (uint64_t)ptr))
        {
            // printf("merged %#018lx  and %#018lx\n", (uint64_t)ptr, (uint64_t)(ptr->prev));
            // 将ptr与前面的空闲块合并
            ptr->prev->length += ptr->length;
            ptr->prev->next = ptr->next;
            if (ptr->next == NULL)
                malloc_free_list_end = ptr->prev;
            else
                ptr->next->prev = ptr->prev;
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
        malloc_free_list_end = ck;
        ck->prev = ck->next = NULL;
        return;
    }
    else
    {

        malloc_mem_chunk_t *ptr = malloc_free_list;
        while (ptr != NULL)
        {
            if ((uint64_t)ptr < (uint64_t)ck)
            {
                if (ptr->next == NULL) // 当前是最后一个项
                {
                    ptr->next = ck;
                    ck->next = NULL;
                    ck->prev = ptr;
                    malloc_free_list_end = ck;
                    break;
                }
                else if ((uint64_t)(ptr->next) > (uint64_t)ck)
                {
                    ck->prev = ptr;
                    ck->next = ptr->next;
                    ptr->next = ck;
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
 * @brief 获取一块堆内存
 *
 * @param size 内存大小
 * @return void* 内存空间的指针
 *
 * 分配内存的时候，结点的prev next指针所占用的空间被当做空闲空间分配出去
 */
void *malloc(ssize_t size)
{
    // printf("malloc\n");
    // 计算需要分配的块的大小
    if (size + sizeof(uint64_t) <= sizeof(malloc_mem_chunk_t))
        size = sizeof(malloc_mem_chunk_t);
    else
        size += sizeof(uint64_t);

    // 采用best fit
    malloc_mem_chunk_t *ck = malloc_query_free_chunk_bf(size);

    if (ck == NULL) // 没有空闲块
    {

        // printf("no free blocks\n");
        // 尝试合并空闲块
        malloc_merge_free_chunk();
        ck = malloc_query_free_chunk_bf(size);

        // 找到了合适的块
        if (ck)
            goto found;
        
        // printf("before enlarge\n");
        // 找不到合适的块，扩容堆区域
        if (malloc_enlarge(size) == -ENOMEM)
            return (void *)-ENOMEM; // 内存不足
        

        malloc_merge_free_chunk(); // 扩容后运行合并，否则会导致碎片

        // 扩容后再次尝试获取

        ck = malloc_query_free_chunk_bf(size);
    }
found:;

    // printf("ck = %#018lx\n", (uint64_t)ck);
    if (ck == NULL)
        return (void *)-ENOMEM;
    // printf("ck->prev=%#018lx ck->next=%#018lx\n", ck->prev, ck->next);
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
    else
        malloc_free_list_end = ck->prev;

    // 当前块剩余的空间还能容纳多一个结点的空间，则分裂当前块
    if ((int64_t)(ck->length) - size > sizeof(malloc_mem_chunk_t))
    {
        // printf("seperate\n");
        malloc_mem_chunk_t *new_ck = (malloc_mem_chunk_t *)(((uint64_t)ck) + size);
        new_ck->length = ck->length - size;
        new_ck->prev = new_ck->next = NULL;
        // printf("new_ck=%#018lx, new_ck->length=%#010lx\n", (uint64_t)new_ck, new_ck->length);
        ck->length = size;
        malloc_insert_free_list(new_ck);
    }
    // printf("malloc done: %#018lx, length=%#018lx\n", ((uint64_t)ck + sizeof(uint64_t)), ck->length);
    // 此时链表结点的指针的空间被分配出去
    return (void *)((uint64_t)ck + sizeof(uint64_t));
}

/**
 * @brief 当堆顶空闲空间大于2个页的空间的时候，释放1个页
 *
 */
static void release_brk()
{
    // 先检测最顶上的块
    // 由于块按照开始地址排列，因此找最后一个块
    if (malloc_free_list_end == NULL)
    {
        printf("release(): free list end is null. \n");
        return;
    }
    if ((uint64_t)malloc_free_list_end + malloc_free_list_end->length == brk_max_addr && (uint64_t)malloc_free_list_end <= brk_max_addr - (PAGE_2M_SIZE << 1))
    {
        int64_t delta = ((brk_max_addr - (uint64_t)malloc_free_list_end) & PAGE_2M_MASK) - PAGE_2M_SIZE;
        // printf("(brk_max_addr - (uint64_t)malloc_free_list_end) & PAGE_2M_MASK=%#018lx\n ", (brk_max_addr - (uint64_t)malloc_free_list_end) & PAGE_2M_MASK);
        // printf("PAGE_2M_SIZE=%#018lx\n", PAGE_2M_SIZE);
        // printf("tdfghgbdfggkmfn=%#018lx\n ", (brk_max_addr - (uint64_t)malloc_free_list_end) & PAGE_2M_MASK - PAGE_2M_SIZE);
        // printf("delta=%#018lx\n ", delta);
        if (delta <= 0) // 不用释放内存
            return;
        sbrk(-delta);
        brk_max_addr = brk(-2);
        brk_managed_addr = brk_max_addr;

        malloc_free_list_end->length = brk_max_addr - (uint64_t)malloc_free_list_end;
    }
}
/**
 * @brief 释放一块堆内存
 *
 * @param ptr 堆内存的指针
 */
void free(void *ptr)
{
    // 找到结点（此时prev和next都处于未初始化的状态）
    malloc_mem_chunk_t *ck = (malloc_mem_chunk_t *)((uint64_t)ptr - sizeof(uint64_t));
    // printf("free(): addr = %#018lx\t len=%#018lx\n", (uint64_t)ck, ck->length);
    count_last_free_size += ck->length;

    malloc_insert_free_list(ck);

    if (count_last_free_size > PAGE_2M_SIZE)
    {
        count_last_free_size = 0;
        malloc_merge_free_chunk();
        release_brk();
    }
}
