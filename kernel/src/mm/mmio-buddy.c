#include "mmio-buddy.h"
#include <mm/slab.h>

/**
 * @brief 将内存对象大小的幂转换成内存池中的数组的下标
 *
 */
#define __exp2index(exp) (exp - 12)

/**
 * @brief 计算伙伴块的内存虚拟地址
 *
 */
#define buddy_block_vaddr(vaddr, exp) (vaddr ^ (1UL << exp))

static struct mmio_buddy_mem_pool __mmio_pool; // mmio buddy内存池

/**
 * @brief 往指定的地址空间链表中添加一个地址区域
 *
 * @param index
 * @param region
 * @return __always_inline
 */
static __always_inline void __buddy_add_region_obj(int index, struct __mmio_buddy_addr_region *region)
{
    struct __mmio_free_region_list *lst = &__mmio_pool.free_regions[index];
    list_init(&region->list);
    list_append(&lst->list_head, &region->list);
    ++lst->num_free;
}

/**
 * @brief 创建新的地址区域结构体
 *
 * @param vaddr 虚拟地址
 * @return 创建好的地址区域结构体
 */
static __always_inline struct __mmio_buddy_addr_region *__mmio_buddy_create_region(uint64_t vaddr)
{
    // 申请内存块的空间
    struct __mmio_buddy_addr_region *region =
        (struct __mmio_buddy_addr_region *)kzalloc(sizeof(struct __mmio_buddy_addr_region), 0);
    list_init(&region->list);
    region->vaddr = vaddr;
    return region;
}

/**
 * @brief 将给定大小为(2^exp)的地址空间一分为二，并插入下一级的链表中
 *
 * @param region 要被分割的地址区域
 * @param exp 要被分割的地址区域的大小的幂
 */
static __always_inline void __buddy_split(struct __mmio_buddy_addr_region *region, int exp)
{
    // 计算分裂出来的新的伙伴块的地址
    struct __mmio_buddy_addr_region *new_region = __mmio_buddy_create_region(buddy_block_vaddr(region->vaddr, exp - 1));
    __buddy_add_region_obj(__exp2index(exp - 1), region);
    __buddy_add_region_obj(__exp2index(exp - 1), new_region);
}

/**
 * @brief 合并两个伙伴块
 *
 * @param x 第一个伙伴块
 * @param y 第二个伙伴块
 * @param exp x、y大小的幂
 * @return int 错误码
 */
static __always_inline int __buddy_merge_blocks(struct __mmio_buddy_addr_region *x, struct __mmio_buddy_addr_region *y,
                                                int exp)
{
    // 判断这两个是否是一对伙伴
    if (unlikely(x->vaddr != buddy_block_vaddr(y->vaddr, exp))) // 不是一对伙伴
        return -EINVAL;

    // === 是一对伙伴，将他们合并
    // 减少计数的工作应在该函数外完成

    // 释放y
    __mmio_buddy_release_addr_region(y);
    // 插入x
    __buddy_add_region_obj(__exp2index(exp + 1), x);

    return 0;
}

/**
 * @brief 从空闲链表中取出指定大小的内存区域, 并从链表中删除
 *
 * @param exp 内存大小的幂
 * @return __always_inline struct* 内存区域结构体
 */
static __always_inline struct __mmio_buddy_addr_region *__buddy_pop_region(int exp)
{
    if (unlikely(list_empty(&__mmio_pool.free_regions[__exp2index(exp)].list_head)))
        return NULL;
    struct __mmio_buddy_addr_region *r = container_of(list_next(&__mmio_pool.free_regions[__exp2index(exp)].list_head),
                                                      struct __mmio_buddy_addr_region, list);
    list_del(&r->list);
    // 区域计数减1
    --__mmio_pool.free_regions[__exp2index(exp)].num_free;
    return r;
}

/**
 * @brief 寻找给定块的伙伴块
 *
 * @param x 给定的内存块
 * @param exp 内存块大小
 * @return 伙伴块的指针
 */
static __always_inline struct __mmio_buddy_addr_region *__find_buddy(struct __mmio_buddy_addr_region *x, int exp)
{
    // 当前为空
    if (unlikely(list_empty(&__mmio_pool.free_regions[__exp2index(exp)].list_head)))
        return NULL;
    // 遍历链表以寻找伙伴块
    uint64_t buddy_vaddr = buddy_block_vaddr(x->vaddr, exp);
    struct List *list = &__mmio_pool.free_regions[__exp2index(exp)].list_head;

    do
    {
        list = list_next(list);
        struct __mmio_buddy_addr_region *bd = container_of(list, struct __mmio_buddy_addr_region, list);
        if (bd->vaddr == buddy_vaddr) // 找到了伙伴块
            return bd;
    } while (list_next(list) != &__mmio_pool.free_regions[__exp2index(exp)].list_head);

    return NULL;
}
/**
 * @brief 把某个大小的伙伴块全都合并成大小为(2^(exp+1))的块
 *
 * @param exp 地址空间大小（2^exp）
 */
static void __buddy_merge(int exp)
{
    struct __mmio_free_region_list *free_list = &__mmio_pool.free_regions[__exp2index(exp)];
    // 若链表为空
    if (list_empty(&free_list->list_head))
        return;

    struct List *list = list_next(&free_list->list_head);

    do
    {
        struct __mmio_buddy_addr_region *ptr = container_of(list, struct __mmio_buddy_addr_region, list);
        // 寻找是否有伙伴块
        struct __mmio_buddy_addr_region *bd = __find_buddy(ptr, exp);

        // 一定要在merge之前执行,否则list就被重置了
        list = list_next(list);

        if (bd != NULL) // 找到伙伴块
        {
            free_list->num_free -= 2;
            list_del(&ptr->list);
            list_del(&bd->list);
            __buddy_merge_blocks(ptr, bd, exp);
        }

    } while (list != &free_list->list_head);
}

/**
 * @brief 从buddy中申请一块指定大小的内存区域
 *
 * @param exp 内存区域的大小(2^exp)
 * @return struct __mmio_buddy_addr_region* 符合要求的内存区域。没有满足要求的时候，返回NULL
 */
struct __mmio_buddy_addr_region *mmio_buddy_query_addr_region(int exp)
{
    if (unlikely(exp > MMIO_BUDDY_MAX_EXP || exp < MMIO_BUDDY_MIN_EXP))
    {
        BUG_ON(1);
        return NULL;
    }
    
    if (!list_empty(&__mmio_pool.free_regions[__exp2index(exp)].list_head))
        goto has_block;

    // 若没有符合要求的内存块，则先尝试分裂大的块
    for (int cur_exp = exp; cur_exp <= MMIO_BUDDY_MAX_EXP; ++cur_exp)
    {
        if (unlikely(
                list_empty(&__mmio_pool.free_regions[__exp2index(cur_exp)].list_head))) // 一直寻找到有空闲空间的链表
            continue;

        // 找到了,逐级向下split
        for (int down_exp = cur_exp; down_exp > exp; --down_exp)
        {
            // 取出一块空闲区域
            struct __mmio_buddy_addr_region *r = __buddy_pop_region(down_exp);
            __buddy_split(r, down_exp);
        }
        break;
    }

    if (!list_empty(&__mmio_pool.free_regions[__exp2index(exp)].list_head))
        goto has_block;

    // 尝试合并小的伙伴块
    for (int cur_exp = MMIO_BUDDY_MIN_EXP; cur_exp < exp; ++cur_exp)
        __buddy_merge(cur_exp);
    // 再次尝试获取符合要求的内存块，若仍不成功，则说明mmio空间耗尽
    if (!list_empty(&__mmio_pool.free_regions[__exp2index(exp)].list_head))
        goto has_block;
    else
        goto failed;
failed:;
    return NULL;
has_block:; // 有可用的内存块，分配
    return __buddy_pop_region(exp);
}

/**
 * @brief 归还一块内存空间到buddy
 *
 * @param vaddr 虚拟地址
 * @param exp 内存空间的大小（2^exp）
 * @return int 返回码
 */
int __mmio_buddy_give_back(uint64_t vaddr, int exp)
{
    // 确保内存对齐，低位都要为0
    if (vaddr & ((1UL << exp) - 1))
        return -EINVAL;

    struct __mmio_buddy_addr_region *region = __mmio_buddy_create_region(vaddr);
    // 加入buddy
    __buddy_add_region_obj(__exp2index(exp), region);
    return 0;
}

/**
 * @brief 初始化mmio的伙伴系统
 *
 */
void mmio_buddy_init()
{
    memset(&__mmio_pool, 0, sizeof(struct mmio_buddy_mem_pool));
    spin_init(&__mmio_pool.op_lock);

    // 初始化各个链表的头部
    for (int i = 0; i < MMIO_BUDDY_REGION_COUNT; ++i)
    {
        list_init(&__mmio_pool.free_regions[i].list_head);
        __mmio_pool.free_regions[i].num_free = 0;
    }

    // 创建一堆1GB的地址块
    uint32_t cnt_1g_blocks = (MMIO_TOP - MMIO_BASE) / PAGE_1G_SIZE;
    uint64_t vaddr_base = MMIO_BASE;
    for (uint32_t i = 0; i < cnt_1g_blocks; ++i, vaddr_base += PAGE_1G_SIZE)
        __mmio_buddy_give_back(vaddr_base, PAGE_1G_SHIFT);
}