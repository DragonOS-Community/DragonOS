#include "mm.h"
#include "slab.h"
#include "../common/printk.h"
#include "../common/kprint.h"
#include "../driver/multiboot2/multiboot2.h"

ul Total_Memory = 0;
ul total_2M_pages = 0;

void mm_init()
{
    kinfo("Initializing memory management unit...");
    // 设置内核程序不同部分的起止地址
    memory_management_struct.kernel_code_start = (ul)&_text;
    memory_management_struct.kernel_code_end = (ul)&_etext;
    memory_management_struct.kernel_data_end = (ul)&_edata;
    memory_management_struct.kernel_end = (ul)&_end;

    struct multiboot_mmap_entry_t *mb2_mem_info;
    int count;
    multiboot2_iter(multiboot2_get_memory, mb2_mem_info, &count);

    for (int i = 0; i < count; ++i)
    {
        //可用的内存
        if (mb2_mem_info->type == 1)
            Total_Memory += mb2_mem_info->len;

        // 保存信息到mms
        memory_management_struct.e820[i].BaseAddr = mb2_mem_info->addr;
        memory_management_struct.e820[i].Length = mb2_mem_info->len;
        memory_management_struct.e820[i].type = mb2_mem_info->type;
        memory_management_struct.len_e820 = i;

        ++mb2_mem_info;

        // 脏数据
        if (mb2_mem_info->type > 4 || mb2_mem_info->len == 0 || mb2_mem_info->type < 1)
            break;
    }
    printk("[ INFO ] Total amounts of RAM : %ld bytes\n", Total_Memory);

    // 计算有效内存页数

    for (int i = 0; i < memory_management_struct.len_e820; ++i)
    {
        if (memory_management_struct.e820[i].type != 1)
            continue;

        // 将内存段的起始物理地址按照2M进行对齐
        ul addr_start = PAGE_2M_ALIGN(memory_management_struct.e820[i].BaseAddr);
        // 将内存段的终止物理地址的低2M区域清空，以实现对齐
        ul addr_end = ((memory_management_struct.e820[i].BaseAddr + memory_management_struct.e820[i].Length) & PAGE_2M_MASK);

        // 内存段不可用
        if (addr_end <= addr_start)
            continue;

        total_2M_pages += ((addr_end - addr_start) >> PAGE_2M_SHIFT);
    }
    kinfo("Total amounts of 2M pages : %ld.", total_2M_pages);

    // 物理地址空间的最大地址（包含了物理内存、内存空洞、ROM等）
    ul max_addr = memory_management_struct.e820[memory_management_struct.len_e820].BaseAddr + memory_management_struct.e820[memory_management_struct.len_e820].Length;
    // 初始化mms的bitmap
    // bmp的指针指向截止位置的4k对齐的上边界（防止修改了别的数据）
    memory_management_struct.bmp = (unsigned long *)((memory_management_struct.kernel_end + PAGE_4K_SIZE - 1) & PAGE_4K_MASK);
    memory_management_struct.bits_size = max_addr >> PAGE_2M_SHIFT;                                                                                         // 物理地址空间的最大页面数
    memory_management_struct.bmp_len = (((unsigned long)(max_addr >> PAGE_2M_SHIFT) + sizeof(unsigned long) * 8 - 1) / 8) & (~(sizeof(unsigned long) - 1)); // bmp由多少个unsigned long变量组成

    // 初始化bitmap， 先将整个bmp空间全部置位。稍后再将可用物理内存页复位。
    memset(memory_management_struct.bmp, 0xff, memory_management_struct.bmp_len);

    // 初始化内存页结构
    // 将页结构映射于bmp之后

    memory_management_struct.pages_struct = (struct Page *)(((unsigned long)memory_management_struct.bmp + memory_management_struct.bmp_len + PAGE_4K_SIZE - 1) & PAGE_4K_MASK);

    memory_management_struct.count_pages = max_addr >> PAGE_2M_SHIFT;
    memory_management_struct.pages_struct_len = ((max_addr >> PAGE_2M_SHIFT) * sizeof(struct Page) + sizeof(long) - 1) & (~(sizeof(long) - 1));
    // 将pages_struct全部清空，以备后续初始化
    memset(memory_management_struct.pages_struct, 0x00, memory_management_struct.pages_struct_len); // init pages memory

    // 初始化内存区域
    memory_management_struct.zones_struct = (struct Zone *)(((ul)memory_management_struct.pages_struct + memory_management_struct.pages_struct_len + PAGE_4K_SIZE - 1) & PAGE_4K_MASK);
    // 由于暂时无法计算zone结构体的数量，因此先将其设为0
    memory_management_struct.count_zones = 0;
    // zones-struct 成员变量暂时按照5个来计算
    memory_management_struct.zones_struct_len = (5 * sizeof(struct Zone) + sizeof(ul) - 1) & (~(sizeof(ul) - 1));
    memset(memory_management_struct.zones_struct, 0x00, memory_management_struct.zones_struct_len);

    // ==== 遍历e820数组，完成成员变量初始化工作 ===

    for (int i = 0; i < memory_management_struct.len_e820; ++i)
    {
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
    ZONE_NORMAL_INDEX = 0;
    ZONE_UNMAPPED_INDEX = 0;

    for (int i = 0; i < memory_management_struct.count_zones; ++i)
    {
        struct Zone *z = memory_management_struct.zones_struct + i;
        // printk_color(ORANGE, BLACK, "zone_addr_start:%#18lx, zone_addr_end:%#18lx, zone_length:%#18lx, pages_group:%#18lx, count_pages:%#18lx\n",
        //             z->zone_addr_start, z->zone_addr_end, z->zone_length, z->pages_group, z->count_pages);

        // 1GB以上的内存空间不做映射
        if (z->zone_addr_start >= 0x100000000 && (!ZONE_UNMAPPED_INDEX))
            ZONE_UNMAPPED_INDEX = i;
    }
    kdebug("ZONE_DMA_INDEX=%d\tZONE_NORMAL_INDEX=%d\tZONE_UNMAPPED_INDEX=%d", ZONE_DMA_INDEX, ZONE_NORMAL_INDEX, ZONE_UNMAPPED_INDEX);
    // 设置内存页管理结构的地址，预留了一段空间，防止内存越界。
    memory_management_struct.end_of_struct = (ul)((ul)memory_management_struct.zones_struct + memory_management_struct.zones_struct_len + sizeof(long) * 32) & (~(sizeof(long) - 1));

    // printk_color(ORANGE, BLACK, "code_start:%#18lx, code_end:%#18lx, data_end:%#18lx, kernel_end:%#18lx, end_of_struct:%#18lx\n",
    //              memory_management_struct.kernel_code_start, memory_management_struct.kernel_code_end, memory_management_struct.kernel_data_end, memory_management_struct.kernel_end, memory_management_struct.end_of_struct);

    // 初始化内存管理单元结构所占的物理页的结构体

    ul mms_max_page = (virt_2_phys(memory_management_struct.end_of_struct) >> PAGE_2M_SHIFT); // 内存管理单元所占据的序号最大的物理页
    kdebug("mms_max_page=%ld", mms_max_page);

    struct Page *tmp_page = NULL;
    ul page_num;
    // 第0个page已经在上方配置
    for (ul j = 1; j <= mms_max_page; ++j)
    {
        tmp_page = memory_management_struct.pages_struct + j;
        page_init(tmp_page, PAGE_PGT_MAPPED | PAGE_KERNEL | PAGE_KERNEL_INIT);
        page_num = tmp_page->addr_phys >> PAGE_2M_SHIFT;
        *(memory_management_struct.bmp + (page_num >> 6)) |= (1UL << (page_num % 64));
        ++tmp_page->zone->count_pages_using;
        --tmp_page->zone->count_pages_free;
    }

    global_CR3 = get_CR3();

    kdebug("global_CR3\t:%#018lx", global_CR3);
    kdebug("*global_CR3\t:%#018lx", *phys_2_virt(global_CR3) & (~0xff));
    kdebug("**global_CR3\t:%#018lx", *phys_2_virt(*phys_2_virt(global_CR3) & (~0xff)) & (~0xff));

    kdebug("1.memory_management_struct.bmp:%#018lx\tzone->count_pages_using:%d\tzone_struct->count_pages_free:%d", *memory_management_struct.bmp, memory_management_struct.zones_struct->count_pages_using, memory_management_struct.zones_struct->count_pages_free);

    kinfo("Memory management unit initialize complete!");

    /*
    kinfo("Cleaning page table remapping at 0x0000");
    for (int i = 0; i < 10; ++i)
        *(phys_2_virt(global_CR3) + i) = 0UL;
    kinfo("Successfully cleaned page table remapping!\n");
    */

    flush_tlb();
    // 初始化slab内存池
    slab_init();
    init_frame_buffer();
    page_table_init();
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
        ++page->zone->total_pages_link;
    }
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

    for (int i = zone_start; i <= zone_end; ++i)
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
                        ++z->count_pages_using;
                        --z->count_pages_free;
                        x->attr = attr;
                    }
                    // 成功分配了页面，返回第一个页面的指针
                    // printk("start page num=%d\n",start_page_num);
                    return (struct Page *)(memory_management_struct.pages_struct + start_page_num);
                }
            }
        }
    }
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
 * 将0~4GB的物理页映射到线性地址空间
 */
void page_table_init()
{
    kinfo("Initializing page table...");
    global_CR3 = get_CR3();
    // 由于CR3寄存器的[11..0]位是PCID标志位，因此将低12位置0后，就是PML4页表的基地址
    ul *pml4_addr = (ul *)((ul)phys_2_virt((ul)global_CR3 & (~0xfffUL)));
    kdebug("PML4 addr=%#018lx *pml4=%#018lx", pml4_addr, *pml4_addr);

    ul *pdpt_addr = phys_2_virt(*pml4_addr & (~0xfffUL));
    kdebug("pdpt addr=%#018lx *pdpt=%#018lx", pdpt_addr, *pdpt_addr);

    ul *pd_addr = phys_2_virt(*pdpt_addr & (~0xfffUL));
    kdebug("pd addr=%#018lx *pd=%#018lx", pd_addr, *pd_addr);

    ul *tmp_addr;
    for (int i = 0; i < memory_management_struct.count_zones; ++i)
    {
        struct Zone *z = memory_management_struct.zones_struct + i;
        struct Page *p = z->pages_group;

        if (i == ZONE_UNMAPPED_INDEX)
            break;

        for (int j = 0; j < z->count_pages; ++j)
        {
            // 计算出PML4页表中的页表项的地址
            tmp_addr = (ul *)((ul)pml4_addr + ((((ul)phys_2_virt(p->addr_phys)) >> PAGE_GDT_SHIFT) & 0x1ff) * 8);

            // 说明该页还没有分配pdpt页表，使用kmalloc分配一个
            if (*tmp_addr = 0)
            {
                ul *virt_addr = kmalloc(PAGE_4K_SIZE, 0);
                set_pml4t(tmp_addr, mk_pml4t(virt_2_phys(virt_addr), PAGE_KERNEL_PGT));
            }

            // 计算出pdpt页表的页表项的地址
            tmp_addr = (ul *)((ul)(phys_2_virt(*tmp_addr & (~0xfffUL))) + ((((ul)phys_2_virt(p->addr_phys)) >> PAGE_1G_SHIFT) & 0x1ff) * 8);

            // 说明该页还没有分配pd页表，使用kmalloc分配一个
            if (*tmp_addr = 0)
            {
                ul *virt_addr = kmalloc(PAGE_4K_SIZE, 0);
                set_pdpt(tmp_addr, mk_pdpt(virt_2_phys(virt_addr), PAGE_KERNEL_DIR));
            }

            // 计算出pd页表的页表项的地址
            tmp_addr = (ul *)((ul)(phys_2_virt(*tmp_addr & (~0xfffUL))) + ((((ul)phys_2_virt(p->addr_phys)) >> PAGE_2M_SHIFT) & 0x1ff) * 8);

            // 填入pd页表的页表项，映射2MB物理页
            set_pdt(tmp_addr, mk_pdt(virt_2_phys(p->addr_phys), PAGE_KERNEL_PAGE));

            // 测试
            if (j % 50 == 0)
                kdebug("pd_addr=%#018lx, *pd_addr=%#018lx", tmp_addr, *tmp_addr);
        }
    }

    flush_tlb();

    kinfo("Page table Initialized.");
}

/**
 * @brief VBE帧缓存区的地址重新映射
 * 将帧缓存区映射到地址0xffff800003000000处
 */
void init_frame_buffer()
{
    kinfo("Re-mapping VBE frame buffer...");
    global_CR3 = get_CR3();
    ul fb_virt_addr = 0xffff800008000000;
    ul fb_phys_addr = get_VBE_FB_phys_addr();

    // 计算帧缓冲区的线性地址对应的pml4页表项的地址
    ul *tmp = phys_2_virt((ul *)((ul)global_CR3 & (~0xfffUL)) + ((fb_virt_addr >> PAGE_GDT_SHIFT) & 0x1ff));
    if (*tmp == 0)
    {
        ul *virt_addr = kmalloc(PAGE_4K_SIZE, 0);
        set_pml4t(tmp, mk_pml4t(virt_2_phys(virt_addr), PAGE_KERNEL_PGT));
    }

    tmp = phys_2_virt((ul *)(*tmp & (~0xfffUL)) + ((fb_virt_addr >> PAGE_1G_SHIFT) & 0x1ff));

    if (*tmp == 0)
    {
        ul *virt_addr = kmalloc(PAGE_4K_SIZE, 0);
        set_pdpt(tmp, mk_pdpt(virt_2_phys(virt_addr), PAGE_KERNEL_DIR));
    }

    ul vbe_fb_length = get_VBE_FB_length();
    ul *tmp1;
    // 初始化2M物理页
    for (ul i = 0; i < (PAGE_2M_SIZE<<3); i += PAGE_2M_SIZE)
    {
        // 计算当前2M物理页对应的pdt的页表项的物理地址
        tmp1 = phys_2_virt((ul *)(*tmp & (~0xfffUL)) + (((ul)(fb_virt_addr + i) >> PAGE_2M_SHIFT) & 0x1ff));

        // 页面写穿，禁止缓存
        set_pdt(tmp1, mk_pdt((ul)fb_phys_addr+i, PAGE_KERNEL_PAGE| PAGE_PWT| PAGE_PCD));
    }

    set_pos_VBE_FB_addr((uint*)fb_virt_addr);
    flush_tlb();
    kinfo("VBE frame buffer successfully Re-mapped!");
}