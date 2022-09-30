#pragma once
#include <common/stddef.h>

#if ARCH(I386) || ARCH(X86_64)
#include <arch/x86_64/asm.h>
#else
#error Arch not supported.
#endif

//链表数据结构
struct List
{
    struct List *prev, *next;
};

//初始化循环链表
static inline void list_init(struct List *list)
{
    list->next = list;
    io_mfence();
    list->prev = list;
}

/**
 * @brief

 * @param entry 给定的节点
 * @param node 待插入的节点
 **/
static inline void list_add(struct List *entry, struct List *node)
{

    node->next = entry->next;
    barrier();
    node->prev = entry;
    barrier();
    node->next->prev = node;
    barrier();
    entry->next = node;
}

/**
 * @brief 将node添加到给定的list的结尾(也就是当前节点的前面)
 * @param entry 列表的入口
 * @param node 待添加的节点
 */
static inline void list_append(struct List *entry, struct List *node)
{

    struct List *tail = entry->prev;
    list_add(tail, node);
}

/**
 * @brief 从列表中删除节点
 * @param entry 待删除的节点
 */
static inline void list_del(struct List *entry)
{

    entry->next->prev = entry->prev;
    entry->prev->next = entry->next;
}

/**
 * @brief 
 * 
 */
#define list_del_init(entry) \
    list_del(entry);         \
    list_init(entry);

/**
 * @brief 将新的链表结点替换掉旧的链表结点，并使得旧的结点的前后指针均为NULL
 *
 * @param old 要被替换的结点
 * @param new 新的要换上去的结点
 */
static inline void list_replace(struct List *old, struct List *new)
{
    if (old->prev != NULL)
        old->prev->next = new;
    new->prev = old->prev;
    if (old->next != NULL)
        old->next->prev = new;
    new->next = old->next;

    old->prev = NULL;
    old->next = NULL;
}

static inline bool list_empty(struct List *entry)
{
    /**
     * @brief 判断循环链表是否为空
     * @param entry 入口
     */

    if (entry == entry->next && entry->prev == entry)
        return true;
    else
        return false;
}

/**
 * @brief 获取链表的上一个元素
 *
 * @param entry
 * @return 链表的上一个元素
 */
static inline struct List *list_prev(struct List *entry)
{
    if (entry->prev != NULL)
        return entry->prev;
    else
        return NULL;
}

/**
 * @brief 获取链表的下一个元素
 *
 * @param entry
 * @return 链表的下一个元素
 */
static inline struct List *list_next(struct List *entry)
{
    if (entry->next != NULL)
        return entry->next;
    else
        return NULL;
}