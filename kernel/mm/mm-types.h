#pragma once
#include <common/glib.h>

struct mm_struct;

/**
 * @brief 内存页表结构体
 *
 */
typedef struct
{
    unsigned long pml4t;
} pml4t_t;

typedef struct
{
    unsigned long pdpt;
} pdpt_t;

typedef struct
{
    unsigned long pdt;
} pdt_t;

typedef struct
{
    unsigned long pt;
} pt_t;

/**
 * @brief 虚拟内存区域(VMA)结构体
 *
 */
struct vm_area_struct
{
    struct List list; // 循环链表结构体

    // 虚拟内存区域的范围是一个左闭右开的区间：[vm_start, vm_end)
    uint64_t vm_start;       // 区域的起始地址
    uint64_t vm_end;         // 区域的结束地址
    struct mm_struct *vm_mm; // 虚拟内存区域对应的mm结构体
    uint64_t vm_flags;       // 虚拟内存区域的标志位

    struct vm_operations_t *vm_ops; // 操作方法
    uint64_t ref_count;             // 引用计数
    void *private_data;
};

/**
 * @brief 内存空间分布结构体
 * 包含了进程内存空间分布的信息
 */
struct mm_struct
{
    pml4t_t *pgd;                // 内存页表指针
    struct vm_area_struct *vmas; // VMA列表
    // 代码段空间
    uint64_t code_addr_start, code_addr_end;
    // 数据段空间
    uint64_t data_addr_start, data_addr_end;
    // 只读数据段空间
    uint64_t rodata_addr_start, rodata_addr_end;
    // BSS段的空间
    uint64_t bss_start, bss_end;
    // 动态内存分配区（堆区域）
    uint64_t brk_start, brk_end;
    // 应用层栈基地址
    uint64_t stack_start;
};