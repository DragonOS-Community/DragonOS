#pragma once
#include <common/glib.h>

struct mm_struct;
typedef uint64_t vm_flags_t;

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

// Address Range Descriptor Structure 地址范围描述符
struct ARDS
{
    ul BaseAddr;           // 基地址
    ul Length;             // 内存长度   以字节为单位
    unsigned int type;     // 本段内存的类型
                           // type=1 表示可以被操作系统使用
                           // type=2 ARR - 内存使用中或被保留，操作系统不能使用
                           // 其他 未定义，操作系统需要将其视为ARR
} __attribute__((packed)); // 修饰该结构体不会生成对齐空间，改用紧凑格式

struct memory_desc
{

    struct ARDS e820[32]; // 物理内存段结构数组
    ul len_e820;          // 物理内存段长度

    ul *bmp;      // 物理空间页映射位图
    ul bmp_len;   //  bmp的长度
    ul bits_size; // 物理地址空间页数量

    struct Page *pages_struct;
    ul count_pages;      // struct page结构体的总数
    ul pages_struct_len; // pages_struct链表的长度

    struct Zone *zones_struct;
    ul count_zones;      // zone结构体的数量
    ul zones_struct_len; // zones_struct列表的长度

    ul kernel_code_start, kernel_code_end; // 内核程序代码段起始地址、结束地址
    ul kernel_data_end, rodata_end;        // 内核程序数据段结束地址、 内核程序只读段结束地址
    uint64_t start_brk;                    // 堆地址的起始位置

    ul end_of_struct; // 内存页管理结构的结束地址
};

struct Zone
{
    // 指向内存页的指针
    struct Page *pages_group;
    ul count_pages; // 本区域的struct page结构体总数

    // 本内存区域的起始、结束的页对齐地址
    ul zone_addr_start;
    ul zone_addr_end;
    ul zone_length; // 区域长度

    // 本区域空间的属性
    ul attr;

    struct memory_desc *gmd_struct;

    // 本区域正在使用中和空闲中的物理页面数量
    ul count_pages_using;
    ul count_pages_free;

    // 物理页被引用次数
    ul total_pages_link;
};

struct Page
{
    // 本页所属的内存域结构体
    struct Zone *zone;
    // 本页对应的物理地址
    ul addr_phys;
    // 页面属性
    ul attr;
    // 页面被引用的次数
    ul ref_counts;
    // 本页的创建时间
    ul age;
};

/**
 * @brief 虚拟内存区域(VMA)结构体
 *
 */
struct vm_area_struct
{
    struct vm_area_struct *vm_prev, *vm_next;

    // 虚拟内存区域的范围是一个左闭右开的区间：[vm_start, vm_end)
    uint64_t vm_start;       // 区域的起始地址
    uint64_t vm_end;         // 区域的结束地址
    struct mm_struct *vm_mm; // 虚拟内存区域对应的mm结构体
    vm_flags_t vm_flags;       // 虚拟内存区域的标志位, 具体可选值请见mm.h

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