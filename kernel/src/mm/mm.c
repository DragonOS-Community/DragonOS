#include "mm.h"
#include "mm-types.h"
#include "mmio.h"
#include "slab.h"
#include <common/printk.h>
#include <common/kprint.h>
#include <driver/multiboot2/multiboot2.h>
#include <process/process.h>
#include <common/compiler.h>
#include <common/errno.h>
#include <debug/traceback/traceback.h>

uint64_t mm_Total_Memory = 0;
uint64_t mm_total_2M_pages = 0;
struct mm_struct initial_mm = {0};

struct memory_desc memory_management_struct = {{0}, 0};

/**
 * @brief 从页表中获取pdt页表项的内容
 *
 * @param proc_page_table_addr 页表的地址
 * @param is_phys 页表地址是否为物理地址
 * @param virt_addr_start 要清除的虚拟地址的起始地址
 * @param length 要清除的区域的长度
 * @param clear 是否清除标志位
 */
uint64_t mm_get_PDE(ul proc_page_table_addr, bool is_phys, ul virt_addr, bool clear);

/**
 * @brief 检查页表是否存在不为0的页表项
 *
 * @param ptr 页表基指针
 * @return int8_t 存在 -> 1
 *                不存在 -> 0
 */
int8_t mm_check_page_table(uint64_t *ptr)
{
    for (int i = 0; i < 512; ++i, ++ptr)
    {
        if (*ptr != 0)
            return 1;
    }
    return 0;
}

void mm_init()
{
    kinfo("Initializing memory management unit...");
    // 设置内核程序不同部分的起止地址
    memory_management_struct.kernel_code_start = (ul)&_text;
    memory_management_struct.kernel_code_end = (ul)&_etext;
    memory_management_struct.kernel_data_end = (ul)&_edata;
    memory_management_struct.rodata_end = (ul)&_erodata;
    memory_management_struct.start_brk = (ul)&_end;

    struct multiboot_mmap_entry_t mb2_mem_info[512];
    int count;

    multiboot2_iter(multiboot2_get_memory, mb2_mem_info, &count);
    io_mfence();
    for (int i = 0; i < count; ++i)
    {
        io_mfence();
        // 可用的内存
        if (mb2_mem_info->type == 1)
            mm_Total_Memory += mb2_mem_info->len;

        // kdebug("[i=%d] mb2_mem_info[i].type=%d, mb2_mem_info[i].addr=%#018lx", i, mb2_mem_info[i].type, mb2_mem_info[i].addr);
        // 保存信息到mms
        memory_management_struct.e820[i].BaseAddr = mb2_mem_info[i].addr;
        memory_management_struct.e820[i].Length = mb2_mem_info[i].len;
        memory_management_struct.e820[i].type = mb2_mem_info[i].type;
        memory_management_struct.len_e820 = i;

        // 脏数据
        if (mb2_mem_info[i].type > 4 || mb2_mem_info[i].len == 0 || mb2_mem_info[i].type < 1)
            break;
    }
    printk("[ INFO ] Total amounts of RAM : %ld bytes\n", mm_Total_Memory);

    // 计算有效内存页数
    io_mfence();
    for (int i = 0; i < memory_management_struct.len_e820; ++i)
    {
        if (memory_management_struct.e820[i].type != 1)
            continue;
        io_mfence();
        // 将内存段的起始物理地址按照2M进行对齐
        ul addr_start = PAGE_2M_ALIGN(memory_management_struct.e820[i].BaseAddr);
        // 将内存段的终止物理地址的低2M区域清空，以实现对齐
        ul addr_end = ((memory_management_struct.e820[i].BaseAddr + memory_management_struct.e820[i].Length) & PAGE_2M_MASK);

        // 内存段不可用
        if (addr_end <= addr_start)
            continue;
        io_mfence();
        mm_total_2M_pages += ((addr_end - addr_start) >> PAGE_2M_SHIFT);
    }
    kinfo("Total amounts of 2M pages : %ld.", mm_total_2M_pages);

    // 物理地址空间的最大地址（包含了物理内存、内存空洞、ROM等）
    ul max_addr = memory_management_struct.e820[memory_management_struct.len_e820].BaseAddr + memory_management_struct.e820[memory_management_struct.len_e820].Length;
    // 初始化mms的bitmap
    // bmp的指针指向截止位置的4k对齐的上边界（防止修改了别的数据）
    io_mfence();
    memory_management_struct.bmp = (unsigned long *)((memory_management_struct.start_brk + PAGE_4K_SIZE - 1) & PAGE_4K_MASK);
    memory_management_struct.bits_size = max_addr >> PAGE_2M_SHIFT;                                                                                         // 物理地址空间的最大页面数
    memory_management_struct.bmp_len = (((unsigned long)(max_addr >> PAGE_2M_SHIFT) + sizeof(unsigned long) * 8 - 1) / 8) & (~(sizeof(unsigned long) - 1)); // bmp由多少个unsigned long变量组成
    io_mfence();

    // 初始化bitmap， 先将整个bmp空间全部置位。稍后再将可用物理内存页复位。
    memset(memory_management_struct.bmp, 0xff, memory_management_struct.bmp_len);
    io_mfence();
    // 初始化内存页结构
    // 将页结构映射于bmp之后
    memory_management_struct.pages_struct = (struct Page *)(((unsigned long)memory_management_struct.bmp + memory_management_struct.bmp_len + PAGE_4K_SIZE - 1) & PAGE_4K_MASK);

    memory_management_struct.count_pages = max_addr >> PAGE_2M_SHIFT;
    memory_management_struct.pages_struct_len = ((max_addr >> PAGE_2M_SHIFT) * sizeof(struct Page) + sizeof(long) - 1) & (~(sizeof(long) - 1));
    // 将pages_struct全部清空，以备后续初始化
    memset(memory_management_struct.pages_struct, 0x00, memory_management_struct.pages_struct_len); // init pages memory

    io_mfence();
    // 初始化内存区域
    memory_management_struct.zones_struct = (struct Zone *)(((ul)memory_management_struct.pages_struct + memory_management_struct.pages_struct_len + PAGE_4K_SIZE - 1) & PAGE_4K_MASK);
    io_mfence();
    // 由于暂时无法计算zone结构体的数量，因此先将其设为0
    memory_management_struct.count_zones = 0;
    io_mfence();
    // zones-struct 成员变量暂时按照5个来计算
    memory_management_struct.zones_struct_len = (10 * sizeof(struct Zone) + sizeof(ul) - 1) & (~(sizeof(ul) - 1));
    io_mfence();
    memset(memory_management_struct.zones_struct, 0x00, memory_management_struct.zones_struct_len);

    // ==== 遍历e820数组，完成成员变量初始化工作 ===

    for (int i = 0; i < memory_management_struct.len_e820; ++i)
    {
        io_mfence();
        if (memory_management_struct.e820[i].type != 1) // 不是操作系统可以使用的物理内存
            continue;
        ul addr_start = PAGE_2M_ALIGN(memory_management_struct.e820[i].BaseAddr);
        ul addr_end = (memory_management_struct.e820[i].BaseAddr + memory_management_struct.e820[i].Length) & PAGE_2M_MASK;

        if (addr_end <= addr_start)
            continue;

        // zone init
        struct Zone *z = memory_management_struct.zones_struct + memory_management_struct.count_zones;
        ++memory_management_struct.count_zones;

        z->zone_addr_start = addr_start;
        z->zone_addr_end = addr_end;
        z->zone_length = addr_end - addr_start;

        z->count_pages_using = 0;
        z->count_pages_free = (addr_end - addr_start) >> PAGE_2M_SHIFT;
        z->total_pages_link = 0;

        z->attr = 0;
        z->gmd_struct = &memory_management_struct;

        z->count_pages = (addr_end - addr_start) >> PAGE_2M_SHIFT;
        z->pages_group = (struct Page *)(memory_management_struct.pages_struct + (addr_start >> PAGE_2M_SHIFT));

        // 初始化页
        struct Page *p = z->pages_group;

        for (int j = 0; j < z->count_pages; ++j, ++p)
        {
            p->zone = z;
            p->addr_phys = addr_start + PAGE_2M_SIZE * j;
            p->attr = 0;

            p->ref_counts = 0;
            p->age = 0;

            // 将bmp中对应的位 复位
            *(memory_management_struct.bmp + ((p->addr_phys >> PAGE_2M_SHIFT) >> 6)) ^= (1UL << ((p->addr_phys >> PAGE_2M_SHIFT) % 64));
        }
    }

    // 初始化0~2MB的物理页
    // 由于这个区间的内存由多个内存段组成，因此不会被以上代码初始化，需要我们手动配置page[0]。
    io_mfence();
    memory_management_struct.pages_struct->zone = memory_management_struct.zones_struct;
    memory_management_struct.pages_struct->addr_phys = 0UL;
    set_page_attr(memory_management_struct.pages_struct, PAGE_PGT_MAPPED | PAGE_KERNEL_INIT | PAGE_KERNEL);
    memory_management_struct.pages_struct->ref_counts = 1;
    memory_management_struct.pages_struct->age = 0;
    // 将第0页的标志位给置上
    //*(memory_management_struct.bmp) |= 1UL;

    // 计算zone结构体的总长度（按照64位对齐）
    memory_management_struct.zones_struct_len = (memory_management_struct.count_zones * sizeof(struct Zone) + sizeof(ul) - 1) & (~(sizeof(ul) - 1));

    ZONE_DMA_INDEX = 0;
    ZONE_NORMAL_INDEX = memory_management_struct.count_zones ;
    ZONE_UNMAPPED_INDEX = 0;

    //kdebug("ZONE_DMA_INDEX=%d\tZONE_NORMAL_INDEX=%d\tZONE_UNMAPPED_INDEX=%d", ZONE_DMA_INDEX, ZONE_NORMAL_INDEX, ZONE_UNMAPPED_INDEX);
    //  设置内存页管理结构的地址，预留了一段空间，防止内存越界。
    memory_management_struct.end_of_struct = (ul)((ul)memory_management_struct.zones_struct + memory_management_struct.zones_struct_len + sizeof(long) * 32) & (~(sizeof(long) - 1));

    // 初始化内存管理单元结构所占的物理页的结构体
    ul mms_max_page = (virt_2_phys(memory_management_struct.end_of_struct) >> PAGE_2M_SHIFT); // 内存管理单元所占据的序号最大的物理页
    // kdebug("mms_max_page=%ld", mms_max_page);

    struct Page *tmp_page = NULL;
    ul page_num;
    // 第0个page已经在上方配置
    for (ul j = 1; j <= mms_max_page; ++j)
    {
        barrier();
        tmp_page = memory_management_struct.pages_struct + j;
        page_init(tmp_page, PAGE_PGT_MAPPED | PAGE_KERNEL | PAGE_KERNEL_INIT);
        barrier();
        page_num = tmp_page->addr_phys >> PAGE_2M_SHIFT;
        *(memory_management_struct.bmp + (page_num >> 6)) |= (1UL << (page_num % 64));
        ++tmp_page->zone->count_pages_using;
        --tmp_page->zone->count_pages_free;
    }

    kinfo("Memory management unit initialize complete!");

    flush_tlb();
    // todo: 在这里增加代码，暂时停止视频输出，否则可能会导致图像数据写入slab的区域，从而造成异常
    // 初始化slab内存池
    slab_init();
    page_table_init();

    initial_mm.pgd = (pml4t_t *)get_CR3();

    initial_mm.code_addr_start = memory_management_struct.kernel_code_start;
    initial_mm.code_addr_end = memory_management_struct.kernel_code_end;

    initial_mm.data_addr_start = (ul)&_data;
    initial_mm.data_addr_end = memory_management_struct.kernel_data_end;

    initial_mm.rodata_addr_start = (ul)&_rodata;
    initial_mm.rodata_addr_end = (ul)&_erodata;
    initial_mm.bss_start = (uint64_t)&_bss;
    initial_mm.bss_end = (uint64_t)&_ebss;

    initial_mm.brk_start = memory_management_struct.start_brk;
    initial_mm.brk_end = current_pcb->addr_limit;

    initial_mm.stack_start = _stack_start;
    initial_mm.vmas = NULL;

    
    
    mmio_init();
}

/**
 * @brief 初始化内存页
 *
 * @param page 内存页结构体
 * @param flags 标志位
 * 本函数只负责初始化内存页，允许对同一页面进行多次初始化
 * 而维护计数器及置位bmp标志位的功能，应当在分配页面的时候手动完成
 * @return unsigned long
 */
unsigned long page_init(struct Page *page, ul flags)
{
    page->attr |= flags;
    // 若页面的引用计数为0或是共享页，增加引用计数
    if ((!page->ref_counts) || (page->attr & PAGE_SHARED))
    {
        ++page->ref_counts;
        barrier();
        if (page->zone)
            ++page->zone->total_pages_link;
    }
    page->anon_vma = NULL;
    spin_init(&(page->op_lock));
    return 0;
}

/**
 * @brief 从已初始化的页结构中搜索符合申请条件的、连续num个struct page
 *
 * @param zone_select 选择内存区域, 可选项：dma, mapped in pgt(normal), unmapped in pgt
 * @param num 需要申请的连续内存页的数量 num<64
 * @param flags 将页面属性设置成flag
 * @return struct Page*
 */
struct Page *alloc_pages(unsigned int zone_select, int num, ul flags)
{
    ul zone_start = 0, zone_end = 0;
    if (num >= 64 && num <= 0)
    {
        kerror("alloc_pages(): num is invalid.");
        return NULL;
    }

    ul attr = flags;
    switch (zone_select)
    {
    case ZONE_DMA:
        // DMA区域
        zone_start = 0;
        zone_end = ZONE_DMA_INDEX;
        attr |= PAGE_PGT_MAPPED;
        break;
    case ZONE_NORMAL:
        zone_start = ZONE_DMA_INDEX;
        zone_end = ZONE_NORMAL_INDEX;
        attr |= PAGE_PGT_MAPPED;
        break;
    case ZONE_UNMAPPED_IN_PGT:
        zone_start = ZONE_NORMAL_INDEX;
        zone_end = ZONE_UNMAPPED_INDEX;
        attr = 0;
        break;

    default:
        kerror("In alloc_pages: param: zone_select incorrect.");
        // 返回空
        return NULL;
        break;
    }

    for (int i = zone_start; i < zone_end; ++i)
    {
        if ((memory_management_struct.zones_struct + i)->count_pages_free < num)
            continue;

        struct Zone *z = memory_management_struct.zones_struct + i;
        // 区域对应的起止页号
        ul page_start = (z->zone_addr_start >> PAGE_2M_SHIFT);
        ul page_end = (z->zone_addr_end >> PAGE_2M_SHIFT);

        ul tmp = 64 - page_start % 64;
        for (ul j = page_start; j < page_end; j += ((j % 64) ? tmp : 64))
        {
            // 按照bmp中的每一个元素进行查找
            // 先将p定位到bmp的起始元素
            ul *p = memory_management_struct.bmp + (j >> 6);

            ul shift = j % 64;
            ul tmp_num = ((1UL << num) - 1);
            for (ul k = shift; k < 64; ++k)
            {
                // 寻找连续num个空页
                if (!((k ? ((*p >> k) | (*(p + 1) << (64 - k))) : *p) & tmp_num))

                {
                    ul start_page_num = j + k - shift; // 计算得到要开始获取的内存页的页号
                    for (ul l = 0; l < num; ++l)
                    {
                        struct Page *x = memory_management_struct.pages_struct + start_page_num + l;

                        // 分配页面，手动配置属性及计数器
                        // 置位bmp
                        *(memory_management_struct.bmp + ((x->addr_phys >> PAGE_2M_SHIFT) >> 6)) |= (1UL << (x->addr_phys >> PAGE_2M_SHIFT) % 64);
                        ++(z->count_pages_using);
                        --(z->count_pages_free);
                        page_init(x, attr);
                    }
                    // 成功分配了页面，返回第一个页面的指针
                    // kwarn("start page num=%d\n", start_page_num);
                    return (struct Page *)(memory_management_struct.pages_struct + start_page_num);
                }
            }
        }
    }
    kBUG("Cannot alloc page, ZONE=%d\tnums=%d, mm_total_2M_pages=%d", zone_select, num, mm_total_2M_pages);
    return NULL;
}

/**
 * @brief 清除页面的引用计数， 计数为0时清空除页表已映射以外的所有属性
 *
 * @param p 物理页结构体
 * @return unsigned long
 */
unsigned long page_clean(struct Page *p)
{
    --p->ref_counts;
    --p->zone->total_pages_link;

    // 若引用计数为空，则清空除PAGE_PGT_MAPPED以外的所有属性
    if (!p->ref_counts)
    {
        p->attr &= PAGE_PGT_MAPPED;
    }
    return 0;
}

/**
 * @brief Get the page's attr
 *
 * @param page 内存页结构体
 * @return ul 属性
 */
ul get_page_attr(struct Page *page)
{
    if (page == NULL)
    {
        kBUG("get_page_attr(): page == NULL");
        return EPAGE_NULL;
    }
    else
        return page->attr;
}

/**
 * @brief Set the page's attr
 *
 * @param page 内存页结构体
 * @param flags  属性
 * @return ul 错误码
 */
ul set_page_attr(struct Page *page, ul flags)
{
    if (page == NULL)
    {
        kBUG("get_page_attr(): page == NULL");
        return EPAGE_NULL;
    }
    else
    {
        page->attr = flags;
        return 0;
    }
}
/**
 * @brief 释放连续number个内存页
 *
 * @param page 第一个要被释放的页面的结构体
 * @param number 要释放的内存页数量 number<64
 */

void free_pages(struct Page *page, int number)
{
    if (page == NULL)
    {
        kerror("free_pages() page is invalid.");
        return;
    }

    if (number >= 64 || number <= 0)
    {
        kerror("free_pages(): number %d is invalid.", number);
        return;
    }

    ul page_num;
    for (int i = 0; i < number; ++i, ++page)
    {
        page_num = page->addr_phys >> PAGE_2M_SHIFT;
        // 复位bmp
        *(memory_management_struct.bmp + (page_num >> 6)) &= ~(1UL << (page_num % 64));
        // 更新计数器
        --page->zone->count_pages_using;
        ++page->zone->count_pages_free;
        page->attr = 0;
    }

    return;
}

/**
 * @brief 重新初始化页表的函数
 * 将所有物理页映射到线性地址空间
 */
void page_table_init()
{
    kinfo("Re-Initializing page table...");
    ul *global_CR3 = get_CR3();

    int js = 0;
    ul *tmp_addr;
    for (int i = 0; i < memory_management_struct.count_zones; ++i)
    {
        struct Zone *z = memory_management_struct.zones_struct + i;
        struct Page *p = z->pages_group;

        if (i == ZONE_UNMAPPED_INDEX && ZONE_UNMAPPED_INDEX != 0)
            break;

        for (int j = 0; j < z->count_pages; ++j)
        {
            mm_map_proc_page_table((uint64_t)get_CR3(), true, (ul)phys_2_virt(p->addr_phys), p->addr_phys, PAGE_2M_SIZE, PAGE_KERNEL_PAGE, false, true, false);

            ++p;
            ++js;
        }
    }

    
    barrier();
        // ========= 在IDLE进程的顶层页表中添加对内核地址空间的映射 =====================

    // 由于IDLE进程的顶层页表的高地址部分会被后续进程所复制，为了使所有进程能够共享相同的内核空间，
    //  因此需要先在IDLE进程的顶层页表内映射二级页表

    uint64_t *idle_pml4t_vaddr = (uint64_t *)phys_2_virt((uint64_t)get_CR3() & (~0xfffUL));

    for (int i = 256; i < 512; ++i)
    {
        uint64_t *tmp = idle_pml4t_vaddr + i;
        barrier();
        if (*tmp == 0)
        {
            void *pdpt = kmalloc(PAGE_4K_SIZE, 0);
            barrier();
            memset(pdpt, 0, PAGE_4K_SIZE);
            barrier();
            set_pml4t(tmp, mk_pml4t(virt_2_phys(pdpt), PAGE_KERNEL_PGT));
        }
    }
    barrier();
    flush_tlb();
    kinfo("Page table Initialized. Affects:%d", js);
}

/**
 * @brief 从页表中获取pdt页表项的内容
 *
 * @param proc_page_table_addr 页表的地址
 * @param is_phys 页表地址是否为物理地址
 * @param virt_addr_start 要清除的虚拟地址的起始地址
 * @param length 要清除的区域的长度
 * @param clear 是否清除标志位
 */
uint64_t mm_get_PDE(ul proc_page_table_addr, bool is_phys, ul virt_addr, bool clear)
{
    ul *tmp;
    if (is_phys)
        tmp = phys_2_virt((ul *)((ul)proc_page_table_addr & (~0xfffUL)) + ((virt_addr >> PAGE_GDT_SHIFT) & 0x1ff));
    else
        tmp = (ul *)((ul)proc_page_table_addr & (~0xfffUL)) + ((virt_addr >> PAGE_GDT_SHIFT) & 0x1ff);

    // pml4页表项为0
    if (*tmp == 0)
        return 0;

    tmp = phys_2_virt((ul *)(*tmp & (~0xfffUL)) + ((virt_addr >> PAGE_1G_SHIFT) & 0x1ff));

    // pdpt页表项为0
    if (*tmp == 0)
        return 0;

    // 读取pdt页表项
    tmp = phys_2_virt(((ul *)(*tmp & (~0xfffUL)) + (((ul)(virt_addr) >> PAGE_2M_SHIFT) & 0x1ff)));

    if (clear) // 清除页表项的标志位
        return *tmp & (~0x1fff);
    else
        return *tmp;
}

/**
 * @brief 从mms中寻找Page结构体
 *
 * @param phys_addr
 * @return struct Page*
 */
static struct Page *mm_find_page(uint64_t phys_addr, uint32_t zone_select)
{
    uint32_t zone_start, zone_end;
    switch (zone_select)
    {
    case ZONE_DMA:
        // DMA区域
        zone_start = 0;
        zone_end = ZONE_DMA_INDEX;
        break;
    case ZONE_NORMAL:
        zone_start = ZONE_DMA_INDEX;
        zone_end = ZONE_NORMAL_INDEX;
        break;
    case ZONE_UNMAPPED_IN_PGT:
        zone_start = ZONE_NORMAL_INDEX;
        zone_end = ZONE_UNMAPPED_INDEX;
        break;

    default:
        kerror("In mm_find_page: param: zone_select incorrect.");
        // 返回空
        return NULL;
        break;
    }

    for (int i = zone_start; i <= zone_end; ++i)
    {
        if ((memory_management_struct.zones_struct + i)->count_pages_using == 0)
            continue;

        struct Zone *z = memory_management_struct.zones_struct + i;

        // 区域对应的起止页号
        ul page_start = (z->zone_addr_start >> PAGE_2M_SHIFT);
        ul page_end = (z->zone_addr_end >> PAGE_2M_SHIFT);

        ul tmp = 64 - page_start % 64;
        for (ul j = page_start; j < page_end; j += ((j % 64) ? tmp : 64))
        {
            // 按照bmp中的每一个元素进行查找
            // 先将p定位到bmp的起始元素
            ul *p = memory_management_struct.bmp + (j >> 6);

            ul shift = j % 64;
            for (ul k = shift; k < 64; ++k)
            {
                if ((*p >> k) & 1) // 若当前页已分配
                {
                    uint64_t page_num = j + k - shift;
                    struct Page *x = memory_management_struct.pages_struct + page_num;

                    if (x->addr_phys == phys_addr) // 找到对应的页
                        return x;
                }
            }
        }
    }
    return NULL;
}

/**
 * @brief 调整堆区域的大小（暂时只能增加堆区域）
 *
 * @todo 缩小堆区域
 * @param old_brk_end_addr 原本的堆内存区域的结束地址
 * @param offset 新的地址相对于原地址的偏移量
 * @return uint64_t
 */
uint64_t mm_do_brk(uint64_t old_brk_end_addr, int64_t offset)
{

    uint64_t end_addr = PAGE_2M_ALIGN(old_brk_end_addr + offset);
    if (offset >= 0)
    {
        for (uint64_t i = old_brk_end_addr; i < end_addr; i += PAGE_2M_SIZE)
        {
            struct vm_area_struct *vma = NULL;
            mm_create_vma(current_pcb->mm, i, PAGE_2M_SIZE, VM_USER | VM_ACCESS_FLAGS, NULL, &vma);
            mm_map(current_pcb->mm, i, PAGE_2M_SIZE, alloc_pages(ZONE_NORMAL, 1, PAGE_PGT_MAPPED)->addr_phys);
            // mm_map_vma(vma, alloc_pages(ZONE_NORMAL, 1, PAGE_PGT_MAPPED)->addr_phys, 0, PAGE_2M_SIZE);
        }
        current_pcb->mm->brk_end = end_addr;
    }
    else
    {

        // 释放堆内存
        for (uint64_t i = end_addr; i < old_brk_end_addr; i += PAGE_2M_SIZE)
        {
            uint64_t phys = mm_get_PDE((uint64_t)phys_2_virt((uint64_t)current_pcb->mm->pgd), false, i, true);

            // 找到对应的页
            struct Page *p = mm_find_page(phys, ZONE_NORMAL);
            if (p == NULL)
            {
                kerror("cannot find page addr=%#018lx", phys);
                return end_addr;
            }

            free_pages(p, 1);
        }

        mm_unmap_proc_table((uint64_t)phys_2_virt((uint64_t)current_pcb->mm->pgd), false, end_addr, PAGE_2M_ALIGN(ABS(offset)));
        // 在页表中取消映射
    }
    return end_addr;
}

/**
 * @brief 创建mmio对应的页结构体
 *
 * @param paddr 物理地址
 * @return struct Page* 创建成功的page
 */
struct Page *__create_mmio_page_struct(uint64_t paddr)
{
    struct Page *p = (struct Page *)kzalloc(sizeof(struct Page), 0);
    if (p == NULL)
        return NULL;
    p->addr_phys = paddr;
    page_init(p, PAGE_DEVICE);
    return p;
}