#pragma once

#if ARCH(I386) || ARCH(X86_64)
#pragma GCC push_options
#pragma GCC optimize("O1")


#include <common/errno.h>
#include <common/spinlock.h>

#if ARCH(I386) || ARCH(X86_64)
#include <arch/x86_64/math/bitcount.h>
#else
#error Arch not supported.
#endif


/**
 * idr: 基于radix-tree的ID-pointer的数据结构
 * 主要功能:
 * 1. 获取一个ID, 并且将该ID与一个指针绑定  - 需要外部加锁
 * 2. 删除一个已分配的ID                  - 需要外部加锁
 * 3. 根据ID查找对应的指针                (读操作,看情况加锁)
 * 4. 根据ID使用新的ptr替换旧的ptr        - 需要外部加锁
 *
 * 附加功能:
 * 1. 给定starting_id, 查询下一个已分配的next_id  (即:next_id>starting_id)
 * 2. 销毁整个idr
 *
 *
 * .... 待实现
 */

// 默认64位机器
#define IDR_BITS 6
#define IDR_FULL 0xfffffffffffffffful

// size = 64
#define IDR_SIZE (1 << IDR_BITS)
#define IDR_MASK ((1 << IDR_BITS) - 1)

// 能管理的ID范围[0:1<<31]
#define MAX_ID_SHIFT (sizeof(int) * 8 - 1)
#define MAX_ID_BIT (1U << MAX_ID_SHIFT)
#define MAX_ID_MASK (MAX_ID_BIT - 1)

// IDR可能最大的层次 以及 IDR预分配空间的最大限制
#define MAX_LEVEL ((MAX_ID_SHIFT + IDR_BITS - 1) / IDR_BITS)
#define IDR_FREE_MAX (MAX_LEVEL << 1)

// 给定layer, 计算完全64叉树的大小
#define TREE_SIZE(layer) ((layer >= 0) ? (1ull << ((layer + 1) * IDR_BITS)) : 1)

// 计算最后(最低位)一个1的位置 (注意使用64位的版本)
#define __lowbit_id(x) ((x) ? (__ctzll(x)) : -1)

// 计算最前(最高位)一个1的位置 (注意使用64位的版本)
#define __mostbit_id(x) ((x) ? (63 - __clzll(x)) : -1)

// radix-tree 节点定义
struct idr_layer
{
    struct idr_layer *ary[IDR_SIZE]; // IDR_SIZE叉树
    unsigned long bitmap;            // 每一位表示这个子树是否被使用
    unsigned long full;              // 64个儿子子树, 每一位代表一个子树是否满了
    int layer;                       // 层数(从底向上)
};

// idr: 将id与pointer绑定的数据结构
struct idr
{
    struct idr_layer *top;
    struct idr_layer *free_list;
    int id_free_cnt;
    spinlock_t lock;
}__attribute__((aligned(8)));

#define DECLARE_IDR(name)    \
    struct idr name = {0};   \
    idr_init(&(name));

#define DECLARE_IDR_LAYER(name)  \
    struct idr_layer name = {0}; \
    memset(name, 0, sizeof(struct idr_layer));

/**
 * 对外函数声明
 **/
int idr_preload(struct idr *idp, gfp_t gfp_mask);
int idr_alloc(struct idr *idp, void *ptr, int *id);
void *idr_remove(struct idr *idp, int id);
void idr_remove_all(struct idr *idp);
void idr_destroy(struct idr *idp);
void *idr_find(struct idr *idp, int id);
void *idr_find_next(struct idr *idp, int start_id);
void *idr_find_next_getid(struct idr *idp, int64_t start_id, int *nextid);
int idr_replace_get_old(struct idr *idp, void *ptr, int id, void **oldptr);
int idr_replace(struct idr *idp, void *ptr, int id);
void idr_init(struct idr *idp);
bool idr_empty(struct idr *idp);
bool idr_count(struct idr *idp, int id);

/**
 * 对外宏：遍历idr两种方式：
 *     1. 从第一个元素开始遍历
 *     2. 从某一个id开始遍历
 */

/**
 * @brief 第一种遍历方式: 从第一个元素开始遍历
 * @param idp idr指针
 * @param id  遍历的id，你不需要初始化这个id，因为它每一次都是从最小已分配的id开始遍历
 * @param ptr 数据指针(entry)，你不需要初始化这个指针
 */
#define for_each_idr_entry(idp, id, ptr) \
    for (id = -1, ptr = idr_find_next_getid(idp, id, &id); ptr != NULL || !idr_count(idp, id); ptr = idr_find_next_getid(idp, id, &id))

/**
 * @brief 第二种遍历方式: 从某一个id开始遍历
 * @param idp idr指针
 * @param id  遍历的id，你需要初始化这个id(请你设置为你要从哪一个id开始遍历，遍历过程将会包括这个id)
 * @param ptr 数据指针(entry)，你不需要初始化这个指针
 */
#define for_each_idr_entry_continue(idp, id, ptr) \
    for (ptr = idr_find_next_getid(idp, id - 1, &id); ptr != NULL || !idr_count(idp, id); ptr = idr_find_next_getid(idp, id, &id))

/**
 * ida: 基于IDR实现的ID分配器
 * 主要功能:
 * 1. 获取一个未分配的ID
 * 2. 询问一个ID是否被分配
 * 3. 删除一个已分配ID
 *
 * 附加功能:
 * 1. 暂定
 */

// 一个块的大小 - 即 sizeof(struct ida_bitmap)
#define IDA_CHUNK_SIZE 128
// ida_bitmap的长度
#define IDA_BITMAP_LONGS (IDA_CHUNK_SIZE / sizeof(long) - 1)
// 对应linux的IDA_BITMAP_BITS = 960 = 15 * 64
#define IDA_FULL (IDA_BITMAP_LONGS * sizeof(long) * 8)
#define IDA_BITMAP_BITS IDA_FULL
#define IDA_BMP_SIZE (8 * sizeof(long))

// 自定义bitmap
struct ida_bitmap
{
    unsigned long count;                    // bitmap中已经分配的id数量
    unsigned long bitmap[IDA_BITMAP_LONGS]; // bitmap本身, 每一个bit代表一个ID
};

// id-allocater 管理+分配ID的数据结构
struct ida
{
    struct idr idr;
    struct ida_bitmap *free_list; // 预分配的数据块
};

#define DECLARE_IDA(name)  \
    struct ida name = {0}; \
    idr_init(&name.idr);   \
    name.free_list = (NULL);

/**
 * 对外函数声明
 */
void ida_init(struct ida *ida_p);
bool ida_empty(struct ida *ida_p);
int ida_preload(struct ida *ida_p, gfp_t gfp_mask);
int ida_alloc(struct ida *ida_p, int *p_id);
bool ida_count(struct ida *ida_p, int id);
void ida_remove(struct ida *ida_p, int id);
void ida_destroy(struct ida *ida_p);

#pragma GCC pop_options

#endif