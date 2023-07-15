#pragma once

#include <asm/current.h>
#include <common/gfp.h>
#include <common/glib.h>
#include <mm/mm-types.h>
#include <process/process.h>

extern void rs_pseudo_map_phys(uint64_t virt_addr, uint64_t phys_addr, uint64_t size);
extern void rs_map_phys(uint64_t virt_addr, uint64_t phys_addr, uint64_t size,uint64_t flags);
extern uint64_t rs_unmap_at_low_addr();

// 每个页表的项数
// 64位下，每个页表4k，每条页表项8B，故一个页表有512条
#define PTRS_PER_PGT 512

// 内核层的起始地址
#define PAGE_OFFSET 0xffff800000000000UL
#define KERNEL_BASE_LINEAR_ADDR 0xffff800000000000UL
#define USER_MAX_LINEAR_ADDR 0x00007fffffffffffUL
// MMIO虚拟地址空间：1TB
#define MMIO_BASE 0xffffa10000000000UL
#define MMIO_TOP 0xffffa20000000000UL

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
#define IO_APIC_MAPPING_OFFSET 0xfec00000UL
#define LOCAL_APIC_MAPPING_OFFSET 0xfee00000UL
#define AHCI_MAPPING_OFFSET 0xff200000UL // AHCI 映射偏移量,之后使用了4M的地址
#define XHCI_MAPPING_OFFSET 0x100000000 // XHCI控制器映射偏移量(后方请预留1GB的虚拟空间来映射不同的controller)

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

// 内核初始化所占用的页 init-code=1 normal-code/data=0
#define PAGE_KERNEL_INIT (1 << 1)

// 1=设备MMIO映射的内存 0=物理内存
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
// 对于PTE而言，第7位是PAT
#define PAGE_4K_PAT (1UL << 7)

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

// 1,0 (4级页表在3级页表中的页表项的属性)
#define PAGE_KERNEL_PDE (PAGE_R_W | PAGE_PRESENT)

// 7,1,0
#define PAGE_KERNEL_PAGE (PAGE_PS | PAGE_R_W | PAGE_PRESENT)

#define PAGE_KERNEL_4K_PAGE (PAGE_R_W | PAGE_PRESENT)

#define PAGE_USER_PGT (PAGE_U_S | PAGE_R_W | PAGE_PRESENT)

// 2,1,0
#define PAGE_USER_DIR (PAGE_U_S | PAGE_R_W | PAGE_PRESENT)

// 1,0 (4级页表在3级页表中的页表项的属性)
#define PAGE_USER_PDE (PAGE_U_S | PAGE_R_W | PAGE_PRESENT)
// 7,2,1,0
#define PAGE_USER_PAGE (PAGE_PS | PAGE_U_S | PAGE_R_W | PAGE_PRESENT)

#define PAGE_USER_4K_PAGE (PAGE_U_S | PAGE_R_W | PAGE_PRESENT)

// ===== 错误码定义 ====
// 物理页结构体为空
#define EPAGE_NULL 1

/**
 * @brief 刷新TLB的宏定义
 * 由于任何写入cr3的操作都会刷新TLB，因此这个宏定义可以刷新TLB
 */
#define flush_tlb()                                                                                                    \
    do                                                                                                                 \
    {                                                                                                                  \
        ul tmp;                                                                                                        \
        io_mfence();                                                                                                   \
        __asm__ __volatile__("movq %%cr3, %0\n\t"                                                                      \
                             "movq %0, %%cr3\n\t"                                                                      \
                             : "=r"(tmp)::"memory");                                                                   \
                                                                                                                       \
    } while (0);

/**
 * @brief 系统内存信息结构体（单位：字节）
 *
 */
struct mm_stat_t
{
    uint64_t total;      // 计算机的总内存数量大小
    uint64_t used;       // 已使用的内存大小
    uint64_t free;       // 空闲物理页所占的内存大小
    uint64_t shared;     // 共享的内存大小
    uint64_t cache_used; // 位于slab缓冲区中的已使用的内存大小
    uint64_t cache_free; // 位于slab缓冲区中的空闲的内存大小
    uint64_t available;  // 系统总空闲内存大小（包括kmalloc缓冲区）
};

/**
 * @brief 虚拟内存区域的操作方法的结构体
 *
 */
struct vm_operations_t
{
    /**
     * @brief vm area 被打开时的回调函数
     *
     */
    void (*open)(struct vm_area_struct *area);
    /**
     * @brief vm area将要被移除的时候，将会调用该回调函数
     *
     */
    void (*close)(struct vm_area_struct *area);
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
int ZONE_NORMAL_INDEX = 0;
int ZONE_UNMAPPED_INDEX = 0;

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
    __asm__ __volatile__("movq %%cr3, %0\n\t" : "=r"(tmp)::"memory");
    return tmp;
}


#define mk_pml4t(addr, attr) ((unsigned long)(addr) | (unsigned long)(attr))
/**
 * @brief 设置pml4页表的页表项
 * @param pml4tptr pml4页表项的地址
 * @param pml4val pml4页表项的值
 */
#define set_pml4t(pml4tptr, pml4tval) (*(pml4tptr) = (pml4tval))

#define mk_pdpt(addr, attr) ((unsigned long)(addr) | (unsigned long)(attr))
#define set_pdpt(pdptptr, pdptval) (*(pdptptr) = (pdptval))

#define mk_pdt(addr, attr) ((unsigned long)(addr) | (unsigned long)(attr))
#define set_pdt(pdtptr, pdtval) (*(pdtptr) = (pdtval))

#define mk_pt(addr, attr) ((unsigned long)(addr) | (unsigned long)(attr))
#define set_pt(ptptr, ptval) (*(ptptr) = (ptval))

/*
 *  vm_area_struct中的vm_flags的可选值
 * 对应的结构体请见mm-types.h
 */
#define VM_NONE 0
#define VM_READ (1 << 0)
#define VM_WRITE (1 << 1)
#define VM_EXEC (1 << 2)
#define VM_SHARED (1 << 3)
#define VM_IO (1 << 4) // MMIO的内存区域
#define VM_SOFTDIRTY (1 << 5)
#define VM_MAYSHARE (1 << 6) // 该vma可被共享
#define VM_USER (1 << 7)     // 该vma可被用户态访问
#define VM_DONTCOPY (1 << 8) // 当fork的时候不拷贝该虚拟内存区域

/* VMA basic access permission flags */
#define VM_ACCESS_FLAGS (VM_READ | VM_WRITE | VM_EXEC)


/**
 * @brief 获取系统当前的内存信息(未上锁，不一定精准)
 *
 * @return struct mm_stat_t 内存信息结构体
 */
struct mm_stat_t mm_stat();
