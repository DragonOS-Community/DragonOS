#pragma once

#include "../common/glib.h"

// 每个页表的项数
// 64位下，每个页表4k，每条页表项8B，故一个页表有512条
#define PTRS_PER_PGT 512

// 内核层的起始地址
#define PAGE_OFFSET (0xffff800000000000UL)
#define KERNEL_BASE_LINEAR_ADDR (0xffff800000000000UL)
#define USER_MAX_LINEAR_ADDR 0x00007fffffffffffUL

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

// 虚拟地址与物理地址转换
#define virt_2_phys(addr) ((unsigned long)(addr)-PAGE_OFFSET)
#define phys_2_virt(addr) ((unsigned long *)((unsigned long)(addr) + PAGE_OFFSET))
// 获取对应的页结构体
#define Virt_To_2M_Page(kaddr) (memory_management_struct.pages_struct + (virt_2_phys(kaddr) >> PAGE_2M_SHIFT))
#define Phy_to_2M_Page(kaddr) (memory_management_struct.pages_struct + ((unsigned long)(kaddr) >> PAGE_2M_SHIFT))

// 在这个地址以上的虚拟空间，用来进行特殊的映射
#define SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE 0xffffa00000000000UL
#define FRAME_BUFFER_MAPPING_OFFSET 0x3000000UL
#define ACPI_RSDT_MAPPING_OFFSET 0x7000000UL
#define ACPI_XSDT_MAPPING_OFFSET 0x9000000UL
#define IO_APIC_MAPPING_OFFSET 0xfec00000UL
#define LOCAL_APIC_MAPPING_OFFSET 0xfee00000UL
#define AHCI_MAPPING_OFFSET 0xff200000UL // AHCI 映射偏移量,之后使用了4M的地址

// ===== 内存区域属性 =====
// DMA区域
#define ZONE_DMA (1 << 0)
// 已在页表中映射的区域
#define ZONE_NORMAL (1 << 1)
// 未在页表中映射的区域
#define ZONE_UNMAPPED_IN_PGT (1 << 2)

// ===== 页面属性 =====
// 页面在页表中已被映射 mapped=1 unmapped=0
#define PAGE_PGT_MAPPED (1 << 0)

// 内核初始化程序的页 init-code=1 normal-code/data=0
#define PAGE_KERNEL_INIT (1 << 1)

// 1=设备寄存器映射的内存 0=物理内存
#define PAGE_DEVICE (1 << 2)

// 内核层页 kernel=1 memory=0
#define PAGE_KERNEL (1 << 3)

// 共享的页 shared=1 single-use=0
#define PAGE_SHARED (1 << 4)

// =========== 页表项权限 ========

//	bit 63	Execution Disable:
#define PAGE_XD (1UL << 63)

//	bit 12	Page Attribute Table
#define PAGE_PAT (1UL << 12)

//	bit 8	Global Page:1,global;0,part
#define PAGE_GLOBAL (1UL << 8)

//	bit 7	Page Size:1,big page;0,small page;
#define PAGE_PS (1UL << 7)

//	bit 6	Dirty:1,dirty;0,clean;
#define PAGE_DIRTY (1UL << 6)

//	bit 5	Accessed:1,visited;0,unvisited;
#define PAGE_ACCESSED (1UL << 5)

//	bit 4	Page Level Cache Disable
#define PAGE_PCD (1UL << 4)

//	bit 3	Page Level Write Through
#define PAGE_PWT (1UL << 3)

//	bit 2	User Supervisor:1,user and supervisor;0,supervisor;
#define PAGE_U_S (1UL << 2)

//	bit 1	Read Write:1,read and write;0,read;
#define PAGE_R_W (1UL << 1)

//	bit 0	Present:1,present;0,no present;
#define PAGE_PRESENT (1UL << 0)

// 1,0
#define PAGE_KERNEL_PGT (PAGE_R_W | PAGE_PRESENT)

// 1,0
#define PAGE_KERNEL_DIR (PAGE_R_W | PAGE_PRESENT)

// 7,1,0
#define PAGE_KERNEL_PAGE (PAGE_PS | PAGE_R_W | PAGE_PRESENT)

#define PAGE_USER_PGT (PAGE_U_S | PAGE_R_W | PAGE_PRESENT)

// 2,1,0
#define PAGE_USER_DIR (PAGE_U_S | PAGE_R_W | PAGE_PRESENT)

// 7,2,1,0
#define PAGE_USER_PAGE (PAGE_PS | PAGE_U_S | PAGE_R_W | PAGE_PRESENT)

// ===== 错误码定义 ====
// 物理页结构体为空
#define EPAGE_NULL 1

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
int ZONE_NORMAL_INDEX = 0;   // low 1GB RAM ,was mapped in pagetable
int ZONE_UNMAPPED_INDEX = 0; // above 1GB RAM,unmapped in pagetable

ul *global_CR3 = NULL;

// 初始化内存管理单元
void mm_init();

/**
 * @brief 初始化内存页
 *
 * @param page 内存页结构体
 * @param flags 标志位
 * 本函数只负责初始化内存页，允许对同一页面进行多次初始化
 * 而维护计数器及置位bmp标志位的功能，应当在分配页面的时候手动完成
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
 * @param num 需要申请的内存页的数量 num<64
 * @param flags 将页面属性设置成flag
 * @return struct Page*
 */
struct Page *alloc_pages(unsigned int zone_select, int num, ul flags);

/**
 * @brief 清除页面的引用计数， 计数为0时清空除页表已映射以外的所有属性
 *
 * @param p 物理页结构体
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
 * @brief Get the page's attr
 *
 * @param page 内存页结构体
 * @return ul 属性
 */
ul get_page_attr(struct Page *page);

/**
 * @brief Set the page's attr
 *
 * @param page 内存页结构体
 * @param flags  属性
 * @return ul 错误码
 */
ul set_page_attr(struct Page *page, ul flags);

/**
 * @brief 内存页表结构体
 *
 */
typedef struct
{
    unsigned long pml4t;
} pml4t_t;
#define mk_pml4t(addr, attr) ((unsigned long)(addr) | (unsigned long)(attr))
/**
 * @brief 设置pml4页表的页表项
 * @param pml4tptr pml4页表项的地址
 * @param pml4val pml4页表项的值
 */
#define set_pml4t(pml4tptr, pml4tval) (*(pml4tptr) = (pml4tval))

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

/**
 * @brief 重新初始化页表的函数
 * 将0~4GB的物理页映射到线性地址空间
 */
void page_table_init();

/**
 * @brief VBE帧缓存区的地址重新映射
 * 将帧缓存区映射到地址0xffff800008000000处
 */
void init_frame_buffer();

/**
 * @brief 将物理地址映射到页表的函数
 *
 * @param virt_addr_start 要映射到的虚拟地址的起始位置
 * @param phys_addr_start 物理地址的起始位置
 * @param length 要映射的区域的长度（字节）
 */
void mm_map_phys_addr(ul virt_addr_start, ul phys_addr_start, ul length, ul flags);

/**
 * @brief 将将物理地址填写到进程的页表的函数
 *
 * @param proc_page_table_addr 页表的基地址
 * @param is_phys 页表的基地址是否为物理地址
 * @param virt_addr_start 要映射到的虚拟地址的起始位置
 * @param phys_addr_start 物理地址的起始位置
 * @param length 要映射的区域的长度（字节）
 * @param user 用户态是否可访问
 */
void mm_map_proc_page_table(ul proc_page_table_addr, bool is_phys, ul virt_addr_start, ul phys_addr_start, ul length, ul flags, bool user);


void mm_map_phys_addr_user(ul virt_addr_start, ul phys_addr_start, ul length, ul flags);