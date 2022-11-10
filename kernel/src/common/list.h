#pragma once
#include <common/stddef.h>

#if ARCH(I386) || ARCH(X86_64)
#include <arch/x86_64/asm/asm.h>
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
 * @brief 删除链表的结点，并将这个结点重新初始化
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

/**
 * @brief 获取当前entry的链表结构体
 *
 * @param ptr 指向List结构体的指针
 * @param type 包裹着List结构体的外层结构体的类型
 * @param member List结构体在上述的“包裹list结构体的结构体”中的变量名
 */
#define list_entry(ptr, type, member) container_of(ptr, type, member)

/**
 * @brief 获取链表中的第一个元素
 * 请注意，该宏要求链表非空，否则会出错
 *
 * @param ptr 指向链表头的指针
 * @param type 包裹着List结构体的外层结构体的类型
 * @param member List结构体在上述的“包裹list结构体的结构体”中的变量名
 */
#define list_first_entry(ptr, type, member) list_entry((ptr)->next, type, member)

/**
 * @brief 获取链表中的第一个元素
 * 若链表为空，则返回NULL
 *
 * @param ptr 指向链表头的指针
 * @param type 包裹着List结构体的外层结构体的类型
 * @param member List结构体在上述的“包裹list结构体的结构体”中的变量名
 */
#define list_first_entry_or_null(ptr, type, member) (!list_empty(ptr) ? list_entry((ptr)->next, type, member) : NULL)

/**
 * @brief 获取链表中的最后一个元素
 * 请注意，该宏要求链表非空，否则会出错
 *
 * @param ptr 指向链表头的指针
 * @param type 包裹着List结构体的外层结构体的类型
 * @param member List结构体在上述的“包裹list结构体的结构体”中的变量名
 */
#define list_last_entry(ptr, type, member) list_entry((ptr)->prev, type, member)

/**
 * @brief 获取链表中的最后一个元素
 * 若链表为空，则返回NULL
 *
 * @param ptr 指向链表头的指针
 * @param type 包裹着List结构体的外层结构体的类型
 * @param member List结构体在上述的“包裹list结构体的结构体”中的变量名
 */
#define list_last_entry_or_full(ptr, type, member) (!list_empty(ptr) ? list_entry((ptr)->prev, type, member) : NULL)

/**
 * @brief 获取链表中的下一个元素
 *
 * @param pos 指向当前的外层结构体的指针
 * @param member 链表结构体在外层结构体内的变量名
 */
#define list_next_entry(pos, member) list_entry((pos)->member.next, typeof(*(pos)), member)

/**
 * @brief 获取链表中的上一个元素
 *
 * @param pos 指向当前的外层结构体的指针
 * @param member 链表结构体在外层结构体内的变量名
 */
#define list_prev_entry(pos, member) list_entry((pos)->member.prev, typeof(*(pos)), member)

/**
 * @brief 遍历整个链表（从前往后）
 *
 * @param ptr the &struct list_head to use as a loop cursor.
 * @param head the head for your list.
 */
#define list_for_each(ptr, head) \
    for ((ptr) = (head)->next; (ptr) != (head); (ptr) = (ptr)->next)

/**
 * @brief 遍历整个链表（从后往前）
 *
 * @param ptr the &struct list_head to use as a loop cursor.
 * @param head the head for your list.
 */
#define list_for_each_prev(ptr, head) \
    for ((ptr) = (head)->prev; (ptr) != (head); (ptr) = (ptr)->prev)

/**
 * @brief 遍历整个链表（从前往后）（支持删除当前链表结点）
 * 该宏通过暂存中间变量，防止在迭代链表的过程中，由于删除了当前ptr所指向的链表结点从而造成错误
 *
 * @param ptr the &struct list_head to use as a loop cursor.
 * @param n 用于存储临时值的List类型的指针
 * @param head the head for your list.
 */
#define list_for_each_safe(ptr, n, head) \
    for ((ptr) = (head)->next, (n) = (ptr)->next; (ptr) != (head); (ptr) = n, n = (ptr)->next)

/**
 * @brief 遍历整个链表（从前往后）（支持删除当前链表结点）
 * 该宏通过暂存中间变量，防止在迭代链表的过程中，由于删除了当前ptr所指向的链表结点从而造成错误
 *
 * @param ptr the &struct list_head to use as a loop cursor.
 * @param n 用于存储临时值的List类型的指针
 * @param head the head for your list.
 */
#define list_for_each_prev_safe(ptr, n, head) \
    for ((ptr) = (head)->prev, (n) = (ptr)->prev; (ptr) != (head); (ptr) = n, n = (ptr)->prev)

/**
 * @brief 从头开始迭代给定类型的链表
 *
 * @param pos 指向特定类型的结构体的指针
 * @param head 链表头
 * @param member struct List在pos的结构体中的成员变量名
 */
#define list_for_each_entry(pos, head, member)               \
    for (pos = list_first_entry(head, typeof(*pos), member); \
         &pos->member != (head);                             \
         pos = list_next_entry(pos, member))

/**
 * @brief 从头开始迭代给定类型的链表（支持删除当前链表结点）
 *
 * @param pos 指向特定类型的结构体的指针
 * @param n 用于存储临时值的，和pos相同类型的指针
 * @param head 链表头
 * @param member struct List在pos的结构体中的成员变量名
 */
#define list_for_each_entry_safe(pos, n, head, member)                                         \
    for (pos = list_first_entry(head, typeof(*pos), member), n = list_next_entry(pos, member); \
         &pos->member != (head);                                                               \
         pos = n, n = list_next_entry(n, member))

/**
 * @brief 逆序迭代给定类型的链表
 *
 * @param pos 指向特定类型的结构体的指针
 * @param head 链表头
 * @param member struct List在pos的结构体中的成员变量名
 */
#define list_for_each_entry_reverse(pos, head, member)      \
    for (pos = list_last_entry(head, typeof(*pos), member); \
         &pos->member != (head);                            \
         pos = list_prev_entry(pos, member))

/**
 * @brief 为list_for_each_entry_continue()准备一个'pos'结构体
 *
 * @param pos 指向特定类型的结构体的，用作迭代起点的指针
 * @param head 指向要开始迭代的struct List结构体的指针
 * @param member struct List在pos的结构体中的成员变量名
 */
#define list_prepare_entry(pos, head, member) \
    ((pos) ? pos : list_entry(head, typeof(*pos), member))

/**
 * @brief 从指定的位置的[下一个元素开始],继续迭代给定的链表
 *
 * @param pos 指向特定类型的结构体的指针。该指针用作迭代的指针。
 * @param head 指向链表头的struct List的指针
 * @param member struct List在pos指向的结构体中的成员变量名
 */
#define list_for_each_entry_continue(pos, head, member) \
    for (pos = list_next_entry(pos, member);            \
         &pos->member != (head);                        \
         pos = list_next_entry(pos, member))

/**
 * @brief 从指定的位置的[下一个元素开始],继续迭代给定的链表。（支持删除当前链表结点）
 *
 * @param pos 指向特定类型的结构体的指针。该指针用作迭代的指针。
 * @param n 用于存储临时值的，和pos相同类型的指针
 * @param head 指向链表头的struct List的指针
 * @param member struct List在pos指向的结构体中的成员变量名
 */
#define list_for_each_entry_safe_continue(pos, n, head, member)                \
    for (pos = list_next_entry(pos, member), n = list_next_entry(pos, member); \
         &pos->member != (head);                                               \
         pos = n, n = list_next_entry(n, member))

/**
 * @brief 从指定的位置的[上一个元素开始],【逆序】迭代给定的链表
 *
 * @param pos 指向特定类型的结构体的指针。该指针用作迭代的指针。
 * @param head 指向链表头的struct List的指针
 * @param member struct List在pos指向的结构体中的成员变量名
 */
#define list_for_each_entry_continue_reverse(pos, head, member) \
    for (pos = list_prev_entry(pos, member);                    \
         &pos->member != (head);                                \
         pos = list_prev_entry(pos, member))

/**
 * @brief 从指定的位置的[上一个元素开始],【逆序】迭代给定的链表。（支持删除当前链表结点）
 *
 * @param pos 指向特定类型的结构体的指针。该指针用作迭代的指针。
 * @param head 指向链表头的struct List的指针
 * @param member struct List在pos指向的结构体中的成员变量名
 */
#define list_for_each_entry_safe_continue_reverse(pos, n, head, member)        \
    for (pos = list_prev_entry(pos, member), n = list_prev_entry(pos, member); \
         &pos->member != (head);                                               \
         pos = n, n = list_prev_entry(n, member))

/**
 * @brief 从指定的位置开始,继续迭代给定的链表
 *
 * @param pos 指向特定类型的结构体的指针。该指针用作迭代的指针。
 * @param head 指向链表头的struct List的指针
 * @param member struct List在pos指向的结构体中的成员变量名
 */
#define list_for_each_entry_from(pos, head, member) \
    for (;                                          \
         &pos->member != (head);                    \
         pos = list_next_entry(pos, member))

/**
 * @brief 从指定的位置开始,继续迭代给定的链表.（支持删除当前链表结点）
 *
 * @param pos 指向特定类型的结构体的指针。该指针用作迭代的指针。
 * @param n 用于存储临时值的，和pos相同类型的指针
 * @param head 指向链表头的struct List的指针
 * @param member struct List在pos指向的结构体中的成员变量名
 */
#define list_for_each_entry_safe_from(pos, n, head, member) \
    for (n = list_next_entry(pos, member);                  \
         &pos->member != (head);                            \
         pos = n, n = list_next_entry(n, member))
