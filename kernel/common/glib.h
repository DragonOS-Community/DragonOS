//
// 内核全局通用库
// Created by longjin on 2022/1/22.
//

#pragma once

//引入对bool类型的支持
#include <stdbool.h>
#include <stdint.h>
#include <common/stddef.h>
#include <arch/arch.h>
#include <common/compiler.h>

#define sti() __asm__ __volatile__("sti\n\t" :: \
                                       : "memory") //开启外部中断
#define cli() __asm__ __volatile__("cli\n\t" :: \
                                       : "memory") //关闭外部中断
#define nop() __asm__ __volatile__("nop\n\t")
#define hlt() __asm__ __volatile__("hlt\n\t")
#define pause() asm volatile("pause\n\t"); // 处理器等待一段时间

//内存屏障
#define io_mfence() __asm__ __volatile__("mfence\n\t" :: \
                                             : "memory") // 在mfence指令前的读写操作必须在mfence指令后的读写操作前完成。
#define io_sfence() __asm__ __volatile__("sfence\n\t" :: \
                                             : "memory") // 在sfence指令前的写操作必须在sfence指令后的写操作前完成
#define io_lfence() __asm__ __volatile__("lfence\n\t" :: \
                                             : "memory") // 在lfence指令前的读操作必须在lfence指令后的读操作前完成。

#define rdtsc() ({                                    \
    uint64_t tmp1 = 0, tmp2 = 0;                      \
    asm volatile("rdtsc"                              \
                 : "=d"(tmp1), "=a"(tmp2)::"memory"); \
    (tmp1 << 32 | tmp2);                              \
})

/**
 * @brief 根据结构体变量内某个成员变量member的基地址，计算出该结构体变量的基地址
 * @param ptr 指向结构体变量内的成员变量member的指针
 * @param type 成员变量所在的结构体
 * @param member 成员变量名
 *
 * 方法：使用ptr减去结构体内的偏移，得到结构体变量的基地址
 */
#define container_of(ptr, type, member)                                     \
    ({                                                                      \
        typeof(((type *)0)->member) *p = (ptr);                             \
        (type *)((unsigned long)p - (unsigned long)&(((type *)0)->member)); \
    })

// 定义类型的缩写
typedef unsigned char uchar;
typedef unsigned short ushort;
typedef unsigned int uint;
typedef unsigned long ul;
typedef unsigned long long int ull;
typedef long long int ll;

#define ABS(x) ((x) > 0 ? (x) : -(x)) // 绝对值
// 最大最小值
#define max(x, y) ((x > y) ? (x) : (y))
#define min(x, y) ((x < y) ? (x) : (y))

// 遮罩高32bit
#define MASK_HIGH_32bit(x) (x & (0x00000000ffffffffUL))

// 四舍五入成整数
ul round(double x)
{
    return (ul)(x + 0.5);
}

/**
 * @brief 地址按照align进行对齐
 *
 * @param addr
 * @param _align
 * @return ul 对齐后的地址
 */
static __always_inline ul ALIGN(const ul addr, const ul _align)
{
    return (ul)((addr + _align - 1) & (~(_align - 1)));
}

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
 * @brief 将新的链表结点替换掉旧的链表结点，并使得旧的结点的前后指针均为NULL
 * 
 * @param old 要被替换的结点
 * @param new 新的要换上去的结点
 */
static inline void list_replace(struct List* old, struct List * new)
{
    if(old->prev!=NULL)
        old->prev->next=new;
    new->prev = old->prev;
    if(old->next!=NULL)
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

void *memset(void *dst, unsigned char C, ul size)
{

    int d0, d1;
    unsigned long tmp = C * 0x0101010101010101UL;
    __asm__ __volatile__("cld	\n\t"
                         "rep	\n\t"
                         "stosq	\n\t"
                         "testb	$4, %b3	\n\t"
                         "je	1f	\n\t"
                         "stosl	\n\t"
                         "1:\ttestb	$2, %b3	\n\t"
                         "je	2f\n\t"
                         "stosw	\n\t"
                         "2:\ttestb	$1, %b3	\n\t"
                         "je	3f	\n\t"
                         "stosb	\n\t"
                         "3:	\n\t"
                         : "=&c"(d0), "=&D"(d1)
                         : "a"(tmp), "q"(size), "0"(size / 8), "1"(dst)
                         : "memory");
    return dst;
}

void *memset_c(void *dst, uint8_t c, size_t count)
{
    uint8_t *xs = (uint8_t *)dst;

    while (count--)
        *xs++ = c;

    return dst;
}

/**
 * @brief 内存拷贝函数
 *
 * @param dst 目标数组
 * @param src 源数组
 * @param Num 字节数
 * @return void*
 */
static void *memcpy(void *dst, const void *src, long Num)
{
    int d0 = 0, d1 = 0, d2 = 0;
    __asm__ __volatile__("cld	\n\t"
                         "rep	\n\t"
                         "movsq	\n\t"
                         "testb	$4,%b4	\n\t"
                         "je	1f	\n\t"
                         "movsl	\n\t"
                         "1:\ttestb	$2,%b4	\n\t"
                         "je	2f	\n\t"
                         "movsw	\n\t"
                         "2:\ttestb	$1,%b4	\n\t"
                         "je	3f	\n\t"
                         "movsb	\n\t"
                         "3:	\n\t"
                         : "=&c"(d0), "=&D"(d1), "=&S"(d2)
                         : "0"(Num / 8), "q"(Num), "1"(dst), "2"(src)
                         : "memory");
    return dst;
}

// 从io口读入8个bit
unsigned char io_in8(unsigned short port)
{
    unsigned char ret = 0;
    __asm__ __volatile__("inb	%%dx,	%0	\n\t"
                         "mfence			\n\t"
                         : "=a"(ret)
                         : "d"(port)
                         : "memory");
    return ret;
}

// 从io口读入32个bit
unsigned int io_in32(unsigned short port)
{
    unsigned int ret = 0;
    __asm__ __volatile__("inl	%%dx,	%0	\n\t"
                         "mfence			\n\t"
                         : "=a"(ret)
                         : "d"(port)
                         : "memory");
    return ret;
}

// 输出8个bit到输出端口
void io_out8(unsigned short port, unsigned char value)
{
    __asm__ __volatile__("outb	%0,	%%dx	\n\t"
                         "mfence			\n\t"
                         :
                         : "a"(value), "d"(port)
                         : "memory");
}

// 输出32个bit到输出端口
void io_out32(unsigned short port, unsigned int value)
{
    __asm__ __volatile__("outl	%0,	%%dx	\n\t"
                         "mfence			\n\t"
                         :
                         : "a"(value), "d"(port)
                         : "memory");
}

/**
 * @brief 从端口读入n个word到buffer
 *
 */
#define io_insw(port, buffer, nr)                                                 \
    __asm__ __volatile__("cld;rep;insw;mfence;" ::"d"(port), "D"(buffer), "c"(nr) \
                         : "memory")

/**
 * @brief 从输出buffer中的n个word到端口
 *
 */
#define io_outsw(port, buffer, nr)                                                 \
    __asm__ __volatile__("cld;rep;outsw;mfence;" ::"d"(port), "S"(buffer), "c"(nr) \
                         : "memory")

/**
 * @brief 读取rsp寄存器的值（存储了页目录的基地址）
 *
 * @return unsigned*  rsp的值的指针
 */
unsigned long *get_rsp()
{
    ul *tmp;
    __asm__ __volatile__(
        "movq %%rsp, %0\n\t"
        : "=r"(tmp)::"memory");
    return tmp;
}

/**
 * @brief 读取rbp寄存器的值（存储了页目录的基地址）
 *
 * @return unsigned*  rbp的值的指针
 */
unsigned long *get_rbp()
{
    ul *tmp;
    __asm__ __volatile__(
        "movq %%rbp, %0\n\t"
        : "=r"(tmp)::"memory");
    return tmp;
}

/**
 * @brief 读取ds寄存器的值（存储了页目录的基地址）
 *
 * @return unsigned*  ds的值的指针
 */
unsigned long *get_ds()
{
    ul *tmp;
    __asm__ __volatile__(
        "movq %%ds, %0\n\t"
        : "=r"(tmp)::"memory");
    return tmp;
}

/**
 * @brief 读取rax寄存器的值（存储了页目录的基地址）
 *
 * @return unsigned*  rax的值的指针
 */
unsigned long *get_rax()
{
    ul *tmp;
    __asm__ __volatile__(
        "movq %%rax, %0\n\t"
        : "=r"(tmp)::"memory");
    return tmp;
}
/**
 * @brief 读取rbx寄存器的值（存储了页目录的基地址）
 *
 * @return unsigned*  rbx的值的指针
 */
unsigned long *get_rbx()
{
    ul *tmp;
    __asm__ __volatile__(
        "movq %%rbx, %0\n\t"
        : "=r"(tmp)::"memory");
    return tmp;
}

// ========= MSR寄存器组操作 =============
/**
 * @brief 向msr寄存器组的address处的寄存器写入值value
 *
 * @param address 地址
 * @param value 要写入的值
 */
void wrmsr(ul address, ul value)
{
    __asm__ __volatile__("wrmsr    \n\t" ::"d"(value >> 32), "a"(value & 0xffffffff), "c"(address)
                         : "memory");
}

/**
 * @brief 从msr寄存器组的address地址处读取值
 * rdmsr返回高32bits在edx，低32bits在eax
 * @param address 地址
 * @return ul address处的寄存器的值
 */
ul rdmsr(ul address)
{
    unsigned int tmp0, tmp1;
    __asm__ __volatile__("rdmsr \n\t"
                         : "=d"(tmp0), "=a"(tmp1)
                         : "c"(address)
                         : "memory");
    return ((ul)tmp0 << 32) | tmp1;
}

uint64_t get_rflags()
{
    unsigned long tmp = 0;
    __asm__ __volatile__("pushfq	\n\t"
                         "movq	(%%rsp), %0	\n\t"
                         "popfq	\n\t"
                         : "=r"(tmp)::"memory");
    return tmp;
}

/**
 * @brief 验证地址空间是否为用户地址空间
 *
 * @param addr_start 地址起始值
 * @param length 地址长度
 * @return true
 * @return false
 */
bool verify_area(uint64_t addr_start, uint64_t length)
{
    if ((addr_start + length) <= 0x00007fffffffffffUL) // 用户程序可用的的地址空间应<= 0x00007fffffffffffUL
        return true;
    else
        return false;
}

/**
 * @brief 从用户空间搬运数据到内核空间
 *
 * @param dst 目的地址
 * @param src 源地址
 * @param size 搬运的大小
 * @return uint64_t
 */
static inline uint64_t copy_from_user(void *dst, void *src, uint64_t size)
{
    uint64_t tmp0, tmp1;
    if (!verify_area((uint64_t)src, size))
        return 0;

    /**
     * @brief 先每次搬运8 bytes，剩余就直接一个个byte搬运
     *
     */
    asm volatile("rep   \n\t"
                 "movsq  \n\t"
                 "movq %3, %0   \n\t"
                 "rep   \n\t"
                 "movsb \n\t"
                 : "=&c"(size), "=&D"(tmp0), "=&S"(tmp1)
                 : "r"(size & 7), "0"(size >> 3), "1"(dst), "2"(src)
                 : "memory");
    return size;
}

/**
 * @brief 从内核空间搬运数据到用户空间
 *
 * @param dst 目的地址
 * @param src 源地址
 * @param size 搬运的大小
 * @return uint64_t
 */
static inline uint64_t copy_to_user(void *dst, void *src, uint64_t size)
{
    uint64_t tmp0, tmp1;
    if (verify_area((uint64_t)src, size))
        return 0;

    /**
     * @brief 先每次搬运8 bytes，剩余就直接一个个byte搬运
     *
     */
    asm volatile("rep   \n\t"
                 "movsq  \n\t"
                 "movq %3, %0   \n\t"
                 "rep   \n\t"
                 "movsb \n\t"
                 : "=&c"(size), "=&D"(tmp0), "=&S"(tmp1)
                 : "r"(size & 7), "0"(size >> 3), "1"(dst), "2"(src)
                 : "memory");
    return size;
}

/**
 * @brief 这个函数让蜂鸣器发声，目前仅用于真机调试。未来将移除，请勿依赖此函数。
 *
 * @param times 发声循环多少遍
 */
void __experimental_beep(uint64_t times);

/**
 * @brief 往指定地址写入8字节
 * 防止由于编译器优化导致不支持的内存访问类型（尤其是在mmio的时候）
 *
 * @param vaddr 虚拟地址
 * @param value 要写入的值
 */
static __always_inline void __write8b(uint64_t vaddr, uint64_t value)
{
    asm volatile("movq %%rdx, 0(%%rax)" ::"a"(vaddr), "d"(value)
                 : "memory");

}

/**
 * @brief 往指定地址写入4字节
 * 防止由于编译器优化导致不支持的内存访问类型（尤其是在mmio的时候）
 *
 * @param vaddr 虚拟地址
 * @param value 要写入的值
 */
static __always_inline void __write4b(uint64_t vaddr, uint32_t value)
{
    asm volatile("movl %%edx, 0(%%rax)" ::"a"(vaddr), "d"(value)
                 : "memory");

}

/**
 * @brief 从指定地址读取8字节
 * 防止由于编译器优化导致不支持的内存访问类型（尤其是在mmio的时候）
 *
 * @param vaddr 虚拟地址
 * @return uint64_t 读取到的值
 */
static __always_inline uint64_t __read8b(uint64_t vaddr)
{
    uint64_t retval;
    asm volatile("movq 0(%%rax), %0"
                 : "=r"(retval)
                 : "a"(vaddr)
                 : "memory");
    return retval;
}

/**
 * @brief 从指定地址读取4字节
 * 防止由于编译器优化导致不支持的内存访问类型（尤其是在mmio的时候）
 *
 * @param vaddr 虚拟地址
 * @return uint64_t 读取到的值
 */
static __always_inline uint32_t __read4b(uint64_t vaddr)
{
    uint32_t retval;
    asm volatile("movl 0(%%rax), %0"
                 : "=d"(retval)
                 : "a"(vaddr)
                 : "memory");
    return retval;
}

/**
 * @brief 将数据从src搬运到dst，并能正确处理地址重叠的问题
 * 
 * @param dst 目标地址指针
 * @param src 源地址指针
 * @param size 大小
 * @return void* 指向目标地址的指针
 */
void *memmove(void *dst, const void *src, uint64_t size);