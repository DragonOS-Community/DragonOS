//
// Created by longjin on 2022/1/20.
//

#include "common/glib.h"
#include "common/printk.h"
#include "common/kprint.h"
#include "exception/gate.h"
#include "exception/trap.h"
#include "exception/irq.h"
#include "mm/mm.h"
#include "mm/slab.h"
#include "process/process.h"
#include "syscall/syscall.h"

unsigned int *FR_address = (unsigned int *)0xb8000; //帧缓存区的地址
                                                    // char fxsave_region[512] __attribute__((aligned(16)));

struct memory_desc memory_management_struct = {{0}, 0};
// struct Global_Memory_Descriptor memory_management_struct = {{0}, 0};

void show_welcome()
{
    /**
     * @brief 打印欢迎页面
     *
     */

    printk("\n\n");
    for (int i = 0; i < 74; ++i)
        printk(" ");
    printk_color(0x00e0ebeb, 0x00e0ebeb, "                                \n");
    for (int i = 0; i < 74; ++i)
        printk(" ");
    printk_color(BLACK, 0x00e0ebeb, "      Welcome to DragonOS !     \n");
    for (int i = 0; i < 74; ++i)
        printk(" ");
    printk_color(0x00e0ebeb, 0x00e0ebeb, "                                \n\n");
}

// 测试内存管理单元
/*
void test_mm()
{
    kinfo("Testing memory management unit...");
    //printk("bmp[0]:%#018x\tbmp[1]%#018lx\n", *memory_management_struct.bmp, *(memory_management_struct.bmp + 1));
    kinfo("Try to allocate 64 memory pages.");
    struct Page *page = alloc_pages(ZONE_NORMAL, 64, PAGE_PGT_MAPPED | PAGE_ACTIVE | PAGE_KERNEL);

    for (int i = 0; i <= 65; ++i)
    {
        printk("page%d\tattr:%#018lx\tphys_addr:%#018lx\t", i, page->attr, page->addr_phys);
        ++page;
        if (((i + 1) % 2) == 0)
            printk("\n");
    }


   printk("bmp[0]:%#018x\tbmp[1]%#018lx\n", *(memory_management_struct.bmp), *(memory_management_struct.bmp + 1));
}
*/

void test_slab()
{
    kinfo("Testing SLAB...");
    kinfo("Testing kmalloc()...");

    for (int i = 1; i < 16; ++i)
    {
        printk_color(ORANGE, BLACK, "mem_obj_size: %ldbytes\t", kmalloc_cache_group[i].size);
        printk_color(ORANGE, BLACK, "bmp(before): %#018lx\t", *kmalloc_cache_group[i].cache_pool->bmp);

        ul *tmp = kmalloc(kmalloc_cache_group[i].size, 0);
        if (tmp == NULL)
        {
            kBUG("Cannot kmalloc such a memory: %ld bytes", kmalloc_cache_group[i].size);
        }

        printk_color(ORANGE, BLACK, "bmp(middle): %#018lx\t", *kmalloc_cache_group[i].cache_pool->bmp);

        kfree(tmp);

        printk_color(ORANGE, BLACK, "bmp(after): %#018lx\n", *kmalloc_cache_group[i].cache_pool->bmp);
    }

    // 测试自动扩容
    kmalloc(kmalloc_cache_group[15].size, 0);
    kmalloc(kmalloc_cache_group[15].size, 0);
    kmalloc(kmalloc_cache_group[15].size, 0);
    kmalloc(kmalloc_cache_group[15].size, 0);
    kmalloc(kmalloc_cache_group[15].size, 0);
    kmalloc(kmalloc_cache_group[15].size, 0);
    kmalloc(kmalloc_cache_group[15].size, 0);


    struct slab_obj *slab_obj_ptr = kmalloc_cache_group[15].cache_pool;
    int count=0;
    do
    {
        kdebug("bmp(%d): addr=%#018lx\t value=%#018lx", count, slab_obj_ptr->bmp, *slab_obj_ptr->bmp);
        
        slab_obj_ptr = container_of(list_next(&slab_obj_ptr->list), struct slab_obj, list);
        ++count;
    } while (slab_obj_ptr != kmalloc_cache_group[15].cache_pool);

    kinfo("SLAB test completed!");
}
// 初始化系统各模块
void system_initialize()
{

    // 初始化printk

    printk_init(8, 16);

    load_TR(10); // 加载TR寄存器
    ul tss_item_addr = 0x7c00;

    set_TSS64(_stack_start, _stack_start, _stack_start, tss_item_addr, tss_item_addr,
              tss_item_addr, tss_item_addr, tss_item_addr, tss_item_addr, tss_item_addr);

    // 初始化中断描述符表
    sys_vector_init();

    //  初始化内存管理单元
    mm_init();

    // 初始化中断模块
    irq_init();

    // 先初始化系统调用模块
    syscall_init();

    cpu_init();

    test_slab();
    // 再初始化进程模块。顺序不能调转
    // process_init();
}

//操作系统内核从这里开始执行
void Start_Kernel(void)
{

    system_initialize();

    // show_welcome();
    // test_mm();

    while (1)
        ;
}

void ignore_int()
{
    kwarn("Unknown interrupt or fault at RIP.\n");
    return;
}