#include "mm.h"
#include "../common/printk.h"

ul Total_Memory = 0;
ul total_2M_pages = 0;

void mm_init()
{
    printk("[ INFO ] Initializing memory management unit...\n");
    // 实模式下获取到的信息的起始地址，转换为ARDS指针
    struct ARDS *ards_ptr = (struct ARDS *)0xffff800000007e00;

    for (int i = 0; i < 32; ++i)
    {
        //printk("Addr = %#18lx\tLength = %#18lx\tType = %#10lx\n",
        //     ards_ptr->BaseAddr, ards_ptr->Length, ards_ptr->type);

        //可用的内存
        if (ards_ptr->type == 1)
            Total_Memory += ards_ptr->Length;

        // 保存信息到mms
        memory_management_struct.e820[i].BaseAddr = ards_ptr->BaseAddr;
        memory_management_struct.e820[i].Length = ards_ptr->Length;
        memory_management_struct.e820[i].type = ards_ptr->type;
        memory_management_struct.len_e820 = i;

        ++ards_ptr;

        // 脏数据
        if (ards_ptr->type > 4 || ards_ptr->Length == 0 || ards_ptr->type < 1)
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
    printk("[ INFO ] Total amounts of 2M pages : %ld.\n", total_2M_pages);

    // 设置内核程序不同部分的起止地址
    memory_management_struct.kernel_code_start = (ul)&_text;
    memory_management_struct.kernel_code_end = (ul)&_etext;
    memory_management_struct.kernel_data_end = (ul)&_edata;
    memory_management_struct.kernel_end = (ul)&_end;

    // 物理地址空间的最大地址（包含了物理内存、内存空洞、ROM等）
    ul max_addr = memory_management_struct.e820[memory_management_struct.len_e820].BaseAddr + memory_management_struct.e820[memory_management_struct.len_e820].Length;
    // 初始化mms的bitmap
    // bmp的指针指向截止位置的4k对齐的上边界（防止修改了别的数据）
    memory_management_struct.bmp = (unsigned long *)((memory_management_struct.kernel_end + PAGE_4K_SIZE - 1) & PAGE_4K_MASK);
    memory_management_struct.bits_size = max_addr >> PAGE_2M_SHIFT;                                                                                         // 物理地址空间的最大页面数
    memory_management_struct.bmp_len = ((unsigned long)((max_addr >> PAGE_2M_SHIFT) + sizeof(unsigned long) * 8 - 1) / 8) & (~(sizeof(unsigned long) - 1)); // bmp由多少个unsigned long变量组成

    // 初始化bitmap， 先将整个bmp空间全部置位。稍后再将可用物理内存页复位。
    memset(memory_management_struct.bmp, 0xff, memory_management_struct.bmp_len);

    // 初始化内存页结构
    // 将页结构映射于bmp之后

    memory_management_struct.pages_struct = (struct Page *)(((unsigned long)memory_management_struct.bmp + memory_management_struct.bmp_len + PAGE_4K_SIZE - 1) & PAGE_4K_MASK);

    memory_management_struct.count_pages = max_addr >> PAGE_2M_SHIFT;
    memory_management_struct.pages_struct_len = ((max_addr >> PAGE_2M_SHIFT) * sizeof(struct Page) + sizeof(long) - 1) & (~(sizeof(long) - 1));
    // 将pages_struct全部清空，以备后续初始化
    memset(memory_management_struct.pages_struct, 0x00, memory_management_struct.pages_struct_len); //init pages memory

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
    memory_management_struct.pages_struct->attr = 0;
    memory_management_struct.pages_struct->ref_counts = 0;
    memory_management_struct.pages_struct->age = 0;

    // 计算zone结构体的总长度（按照64位对齐）
    memory_management_struct.zones_struct_len = (memory_management_struct.count_zones * sizeof(struct Zone) + sizeof(ul) - 1) & (~(sizeof(ul) - 1));

    printk_color(ORANGE, BLACK, "bmp:%#18lx, bmp_len:%#18lx, bits_size:%#18lx\n", memory_management_struct.bmp, memory_management_struct.bmp_len, memory_management_struct.bits_size);

    printk_color(ORANGE, BLACK, "pages_struct:%#18lx, count_pages:%#18lx, pages_struct_len:%#18lx\n", memory_management_struct.pages_struct, memory_management_struct.count_pages, memory_management_struct.pages_struct_len);

    printk_color(ORANGE, BLACK, "zones_struct:%#18lx, count_zones:%#18lx, zones_struct_len:%#18lx\n", memory_management_struct.zones_struct, memory_management_struct.count_zones, memory_management_struct.zones_struct_len);

    ZONE_DMA_INDEX = 0;    //need rewrite in the future
    ZONE_NORMAL_INDEX = 0; //need rewrite in the future

    for (int i = 0; i < memory_management_struct.count_zones; ++i) //need rewrite in the future
    {
        struct Zone *z = memory_management_struct.zones_struct + i;
        printk_color(ORANGE, BLACK, "zone_addr_start:%#18lx, zone_addr_end:%#18lx, zone_length:%#18lx, pages_group:%#18lx, count_pages:%#18lx\n",
                     z->zone_addr_start, z->zone_addr_end, z->zone_length, z->pages_group, z->count_pages);

        // 1GB以上的内存空间不做映射
        if (z->zone_addr_start == 0x100000000)
            ZONE_UNMAPED_INDEX = i;
    }
    // 设置内存页管理结构的地址，预留了一段空间，防止内存越界。
    memory_management_struct.end_of_struct = (ul)((ul)memory_management_struct.zones_struct + memory_management_struct.zones_struct_len + sizeof(long) * 32) & (~(sizeof(long) - 1));

    printk_color(ORANGE, BLACK, "code_start:%#18lx, code_end:%#18lx, data_end:%#18lx, kernel_end:%#18lx, end_of_struct:%#18lx\n",
                 memory_management_struct.kernel_code_start, memory_management_struct.kernel_code_end, memory_management_struct.kernel_data_end, memory_management_struct.kernel_end, memory_management_struct.end_of_struct);

    // 初始化内存管理单元结构所占的物理页的结构体

    ul mms_max_page = (virt_2_phys(memory_management_struct.end_of_struct) >> PAGE_2M_SHIFT); // 内存管理单元所占据的序号最大的物理页

    for (ul j = 0; j <= mms_max_page; ++j)
    {
        page_init(memory_management_struct.pages_struct + j, PAGE_PGT_MAPPED | PAGE_KERNEL | PAGE_KERNEL_INIT | PAGE_ACTIVE);
    }

    ul *cr3 = get_CR3();

    printk_color(INDIGO, BLACK, "cr3:\t%#018lx\n", cr3);
    printk_color(INDIGO, BLACK, "*cr3:\t%#018lx\n", *(phys_2_virt(cr3)) & (~0xff));
    printk_color(INDIGO, BLACK, "**cr3:\t%#018lx\n", *phys_2_virt(*(phys_2_virt(cr3)) & (~0xff)) & (~0xff));

    // 消除一致性页表映射，将页目录（PML4E）的前10项清空
    for (int i = 0; i < 10; ++i)
        *(phys_2_virt(cr3) + i) = 0UL;

    flush_tlb();

    printk("[ INFO ] Memory management unit initialized.\n");
}

/**
 * @brief 初始化内存页
 * 
 * @param page 内存页结构体
 * @param flags 标志位
 * 对于新页面： 初始化struct page
 * 对于当前页面属性/flags中含有引用属性或共享属性时，则只增加struct page和struct zone的被引用计数。否则就只是添加页表属性，并置位bmp的相应位。
 * @return unsigned long 
 */
unsigned long page_init(struct Page *page, ul flags)
{
    // 全新的页面
    if (!page->attr)
    {
        // 将bmp对应的标志位置位
        *(memory_management_struct.bmp + ((page->addr_phys >> PAGE_2M_SHIFT) >> 6)) |= (1UL << ((page->addr_phys >> PAGE_2M_SHIFT) % 64));

        page->attr = flags;
        ++(page->ref_counts);
        ++(page->zone->count_pages_using);
        --(page->zone->count_pages_free);
        ++(page->zone->total_pages_link);
    }
    // 不是全新的页面，而是含有引用属性/共享属性
    else if ((page->attr & PAGE_REFERENCED) || (page->attr & PAGE_K_SHARE_TO_U) || (flags & PAGE_REFERENCED) || (flags & PAGE_K_SHARE_TO_U))
    {
        page->attr |= flags;
        ++(page->ref_counts);
        ++(page->zone->total_pages_link);
    }
    else
    {
        // 将bmp对应的标志位置位
        *(memory_management_struct.bmp + ((page->addr_phys >> PAGE_2M_SHIFT) >> 6)) |= (1UL << ((page->addr_phys >> PAGE_2M_SHIFT) % 64));
        page->attr |= flags;
    }
    return 0;
}

/**
 * @brief 从已初始化的页结构中搜索符合申请条件的、连续num个struct page
 * 
 * @param zone_select 选择内存区域, 可选项：dma, mapped in pgt, unmapped in pgt
 * @param num 需要申请的连续内存页的数量 num<=64
 * @param flags 将页面属性设置成flag
 * @return struct Page* 
 */
struct Page *alloc_pages(unsigned int zone_select, int num, ul flags)
{
    ul zone_start = 0, zone_end = 0;
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
        zone_end = ZONE_UNMAPED_INDEX;
        break;

    default:
        printk("[ ");
        printk_color(YELLOW, BLACK, "WARN");
        printk(" ] In alloc_pages: param: zone_select incorrect.\n");
        // 返回空
        return NULL;
        break;
    }

    for (int i = zone_start; i <= zone_end; ++i)
    {
        if ((memory_management_struct.zones_struct + i)->count_pages_free < num)
            continue;

        struct Zone *z = memory_management_struct.zones_struct + i;

        // 区域对应的起止页号以及区域拥有的页面数
        ul page_start = (z->zone_addr_start >> PAGE_2M_SHIFT);
        ul page_end = (z->zone_addr_end >> PAGE_2M_SHIFT);
        ul page_num = (z->zone_length >> PAGE_2M_SHIFT);

        ul tmp = 64 - page_start % 64;
        for (ul j = page_start; j <= page_end; j += ((j % 64) ? tmp : 64))
        {
            // 按照bmp中的每一个元素进行查找
            // 先将p定位到bmp的起始元素
            ul *p = memory_management_struct.bmp + (j >> 6);

            ul shift = j % 64;

            for (int k = shift; k < 64 - shift; ++k)
            {
                // 寻找连续num个空页
                if (!(((*p >> k) | (*(p + 1) << (64 - k))) & (num == 64 ? 0xffffffffffffffffUL : ((1 << num) - 1))))
                {
                    ul start_page_num = j + k - shift; // 计算得到要开始获取的内存页的页号（书上的公式有问题，这个是改过之后的版本）
                    for(int l=0;l<num;++l)
                    {
                        struct Page* x = memory_management_struct.pages_struct+start_page_num+l;
                        
                        page_init(x, flags);
                    }
                    // 成功分配了页面，返回第一个页面的指针
                    return (struct Page*)(memory_management_struct.pages_struct+start_page_num);
                }
            }
        }
    }
}