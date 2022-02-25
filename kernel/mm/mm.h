#pragma once

#include "../common/glib.h"

// 每个页表的项数
// 64位下，每个页表4k，每条页表项8B，故一个页表有512条
#define PTRS_PER_PGT 512

// 内核层的起始地址
#define PAGE_OFFSET ((unsigned long)0x000000)
#define KERNEL_BASE_PHYS_ADDR ((unsigned long)0x100000)

#define PAGE_4K_SHIFT 12
#define PAGE_2M_SHIFT 21
#define PAGE_1G_SHIFT 30

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

// 虚拟地址与物理地址转换
#define virt_2_phys(addr) ((unsigned long)(addr)-PAGE_OFFSET)
#define phys_2_virt(addr) ((unsigned long *)((unsigned long)(addr) + PAGE_OFFSET))
// 获取对应的页结构体
#define Virt_To_2M_Page(kaddr) (memory_management_struct.pages_struct + (virt_2_phys(kaddr) >> PAGE_2M_SHIFT))
#define Phy_to_2M_Page(kaddr) (memory_management_struct.pages_struct + ((unsigned long)(kaddr) >> PAGE_2M_SHIFT))

// ===== 内存区域属性 =====
// DMA区域
#define ZONE_DMA (1 << 0)
// 已在页表中映射的区域
#define ZONE_NORMAL (1 << 1)
// 未在页表中映射的区域
#define ZONE_UNMAPPED_IN_PGT (1 << 2)

// ===== 页面属性 =====
// 页面在页表中已被映射
#define PAGE_PGT_MAPPED (1 << 0)
// 内核初始化程序的页
#define PAGE_KERNEL_INIT (1 << 1)
// 引用的页
#define PAGE_REFERENCED (1 << 2)
// 脏页
#define PAGE_DIRTY (1 << 3)
// 使用中的页
#define PAGE_ACTIVE (1 << 4)
// 过时的页
#define PAGE_UP_TO_DATE (1 << 5)
// 设备对应的页
#define PAGE_DEVICE (1 << 6)
// 内核层页
#define PAGE_KERNEL (1 << 7)
// 内核共享给用户态程序的页面
#define PAGE_K_SHARE_TO_U (1 << 8)
// slab内存分配器的页
#define PAGE_SLAB (1 << 9)

/**
 * @brief 刷新TLB的宏定义
 * 由于任何写入cr3的操作都会刷新TLB，因此这个宏定义可以刷新TLB
 */
#define flush_tlb()                 \
    do                              \
    {                               \
        ul tmp;                     \
        __asm__ __volatile__(       \
            "movq %%cr3, %0\n\t"    \
            "movq %0, %%cr3\n\t"    \
            : "=r"(tmp)::"memory"); \
                                    \
    } while (0);

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
    ul kernel_data_end, kernel_end;        // 内核程序数据段结束地址、 内核程序结束地址

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

extern struct memory_desc memory_management_struct;

// 导出内核程序的几个段的起止地址
extern char _text;
extern char _etext;
extern char _data;
extern char _edata;
extern char _rodata;
extern char _erodata;
extern char _bss;
extern char _ebss;
extern char _end;

// 每个区域的索引

int ZONE_DMA_INDEX = 0;
int ZONE_NORMAL_INDEX = 0;  // low 1GB RAM ,was mapped in pagetable
int ZONE_UNMAPED_INDEX = 0; // above 1GB RAM,unmapped in pagetable

ul *global_CR3 = NULL;

// 初始化内存管理单元
void mm_init();

/**
 * @brief 初始化内存页
 *
 * @param page 内存页结构体
 * @param flags 标志位
 * 对于新页面： 初始化struct page
 * 对于当前页面属性/flags中含有引用属性或共享属性时，则只增加struct page和struct zone的被引用计数。否则就只是添加页表属性，并置位bmp的相应位。
 * @return unsigned long
 */
unsigned long page_init(struct Page *page, ul flags);

/**
 * @brief 读取CR3寄存器的值（存储了页目录的基地址）
 *
 * @return unsigned*  cr3的值的指针
 */
unsigned long *get_CR3()
{
    ul *tmp;
    __asm__ __volatile__(
        "movq %%cr3, %0\n\t"
        : "=r"(tmp)::"memory");
    return tmp;
}

/**
 * @brief 从已初始化的页结构中搜索符合申请条件的、连续num个struct page
 *
 * @param zone_select 选择内存区域, 可选项：dma, mapped in pgt(normal), unmapped in pgt
 * @param num 需要申请的内存页的数量 num<=64
 * @param flags 将页面属性设置成flag
 * @return struct Page*
 */
struct Page *alloc_pages(unsigned int zone_select, int num, ul flags);

/**
 * @brief 释放内存页
 *
 * @param page 内存页结构体
 * @return unsigned long
 */
unsigned long page_clean(struct Page *page);

/**
 * @brief 释放连续number个内存页
 *
 * @param page 第一个要被释放的页面的结构体
 * @param number 要释放的内存页数量 number<64
 */
void free_pages(struct Page *page, int number);

/**
 * @brief 内存页表结构体
 *
 */
typedef struct
{
    unsigned long pml4t;
} pml4t_t;
#define mk_pml4t(addr, attr) ((unsigned long)(addr) | (unsigned long)(attr))
#define set_pml4t(mpl4tptr, mpl4tval) (*(mpl4tptr) = (mpl4tval))

typedef struct
{
    unsigned long pdpt;
} pdpt_t;
#define mk_pdpt(addr, attr) ((unsigned long)(addr) | (unsigned long)(attr))
#define set_pdpt(pdptptr, pdptval) (*(pdptptr) = (pdptval))

typedef struct
{
    unsigned long pdt;
} pdt_t;
#define mk_pdt(addr, attr) ((unsigned long)(addr) | (unsigned long)(attr))
#define set_pdt(pdtptr, pdtval) (*(pdtptr) = (pdtval))

typedef struct
{
    unsigned long pt;
} pt_t;
#define mk_pt(addr, attr) ((unsigned long)(addr) | (unsigned long)(attr))
#define set_pt(ptptr, ptval) (*(ptptr) = (ptval))