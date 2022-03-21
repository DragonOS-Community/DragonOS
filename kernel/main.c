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

#include "driver/multiboot2/multiboot2.h"
#include "driver/acpi/acpi.h"
#include "driver/keyboard/ps2_keyboard.h"
#include "driver/mouse/ps2_mouse.h"
#include "driver/disk/ata.h"

unsigned int *FR_address = (unsigned int *)0xb8000; //帧缓存区的地址

struct memory_desc memory_management_struct = {{0}, 0};
// struct Global_Memory_Descriptor memory_management_struct = {{0}, 0};
void test_slab();
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

void test_mm()
{
    kinfo("Testing memory management unit...");
    struct Page *page = NULL;
    page = alloc_pages(ZONE_NORMAL, 63, 0);
    page = alloc_pages(ZONE_NORMAL, 63, 0);

    printk_color(ORANGE, BLACK, "4.memory_management_struct.bmp:%#018lx\tmemory_management_struct.bmp+1:%#018lx\tmemory_management_struct.bmp+2:%#018lx\tzone_struct->count_pages_using:%d\tzone_struct->count_pages_free:%d\n", *memory_management_struct.bmp, *(memory_management_struct.bmp + 1), *(memory_management_struct.bmp + 2), memory_management_struct.zones_struct->count_pages_using, memory_management_struct.zones_struct->count_pages_free);

    for (int i = 80; i <= 85; ++i)
    {
        printk_color(INDIGO, BLACK, "page%03d attr:%#018lx address:%#018lx\t", i, (memory_management_struct.pages_struct + i)->attr, (memory_management_struct.pages_struct + i)->addr_phys);
        i++;
        printk_color(INDIGO, BLACK, "page%03d attr:%#018lx address:%#018lx\n", i, (memory_management_struct.pages_struct + i)->attr, (memory_management_struct.pages_struct + i)->addr_phys);
    }

    for (int i = 140; i <= 145; i++)
    {
        printk_color(INDIGO, BLACK, "page%03d attr:%#018lx address:%#018lx\t", i, (memory_management_struct.pages_struct + i)->attr, (memory_management_struct.pages_struct + i)->addr_phys);
        i++;
        printk_color(INDIGO, BLACK, "page%03d attr:%#018lx address:%#018lx\n", i, (memory_management_struct.pages_struct + i)->attr, (memory_management_struct.pages_struct + i)->addr_phys);
    }

    free_pages(page, 1);

    printk_color(ORANGE, BLACK, "5.memory_management_struct.bmp:%#018lx\tmemory_management_struct.bmp+1:%#018lx\tmemory_management_struct.bmp+2:%#018lx\tzone_struct->count_pages_using:%d\tzone_struct->count_pages_free:%d\n", *memory_management_struct.bmp, *(memory_management_struct.bmp + 1), *(memory_management_struct.bmp + 2), memory_management_struct.zones_struct->count_pages_using, memory_management_struct.zones_struct->count_pages_free);

    for (int i = 75; i <= 85; i++)
    {
        printk_color(INDIGO, BLACK, "page%03d attr:%#018lx address:%#018lx\t", i, (memory_management_struct.pages_struct + i)->attr, (memory_management_struct.pages_struct + i)->addr_phys);
        i++;
        printk_color(INDIGO, BLACK, "page%03d attr:%#018lx address:%#018lx\n", i, (memory_management_struct.pages_struct + i)->attr, (memory_management_struct.pages_struct + i)->addr_phys);
    }

    page = alloc_pages(ZONE_UNMAPPED_IN_PGT, 63, 0);

    printk_color(ORANGE, BLACK, "6.memory_management_struct.bmp:%#018lx\tmemory_management_struct.bmp+1:%#018lx\tzone_struct->count_pages_using:%d\tzone_struct->count_pages_free:%d\n", *(memory_management_struct.bmp + (page->addr_phys >> PAGE_2M_SHIFT >> 6)), *(memory_management_struct.bmp + 1 + (page->addr_phys >> PAGE_2M_SHIFT >> 6)), (memory_management_struct.zones_struct + ZONE_UNMAPPED_INDEX)->count_pages_using, (memory_management_struct.zones_struct + ZONE_UNMAPPED_INDEX)->count_pages_free);

    free_pages(page, 1);

    printk_color(ORANGE, BLACK, "7.memory_management_struct.bmp:%#018lx\tmemory_management_struct.bmp+1:%#018lx\tzone_struct->count_pages_using:%d\tzone_struct->count_pages_free:%d\n", *(memory_management_struct.bmp + (page->addr_phys >> PAGE_2M_SHIFT >> 6)), *(memory_management_struct.bmp + 1 + (page->addr_phys >> PAGE_2M_SHIFT >> 6)), (memory_management_struct.zones_struct + ZONE_UNMAPPED_INDEX)->count_pages_using, (memory_management_struct.zones_struct + ZONE_UNMAPPED_INDEX)->count_pages_free);

    test_slab();
    kinfo("Memory management module test completed!");
}

void test_slab()
{
    kinfo("Testing SLAB...");
    kinfo("Testing kmalloc()...");

    for (int i = 1; i < 16; ++i)
    {
        printk_color(ORANGE, BLACK, "mem_obj_size: %ldbytes\t", kmalloc_cache_group[i].size);
        printk_color(ORANGE, BLACK, "bmp(before): %#018lx\t", *kmalloc_cache_group[i].cache_pool_entry->bmp);

        ul *tmp = kmalloc(kmalloc_cache_group[i].size, 0);
        if (tmp == NULL)
        {
            kBUG("Cannot kmalloc such a memory: %ld bytes", kmalloc_cache_group[i].size);
        }

        printk_color(ORANGE, BLACK, "bmp(middle): %#018lx\t", *kmalloc_cache_group[i].cache_pool_entry->bmp);

        kfree(tmp);

        printk_color(ORANGE, BLACK, "bmp(after): %#018lx\n", *kmalloc_cache_group[i].cache_pool_entry->bmp);
    }

    // 测试自动扩容
    void *ptrs[7];
    for (int i = 0; i < 7; ++i)
        ptrs[i] = kmalloc(kmalloc_cache_group[15].size, 0);

    struct slab_obj *slab_obj_ptr = kmalloc_cache_group[15].cache_pool_entry;
    int count = 0;
    do
    {
        kdebug("bmp(%d): addr=%#018lx\t value=%#018lx", count, slab_obj_ptr->bmp, *slab_obj_ptr->bmp);

        slab_obj_ptr = container_of(list_next(&slab_obj_ptr->list), struct slab_obj, list);
        ++count;
    } while (slab_obj_ptr != kmalloc_cache_group[15].cache_pool_entry);

    for (int i = 0; i < 7; ++i)
        kfree(ptrs[i]);

    kinfo("SLAB test completed!");
}
// 初始化系统各模块
void system_initialize()
{

    // 初始化printk
    printk_init(8, 16);

    kinfo("Kernel Starting...");

    load_TR(10); // 加载TR寄存器
    ul tss_item_addr = 0x7c00;

    set_TSS64(_stack_start, _stack_start, _stack_start, tss_item_addr, tss_item_addr,
              tss_item_addr, tss_item_addr, tss_item_addr, tss_item_addr, tss_item_addr);

    // 初始化中断描述符表
    sys_vector_init();

    //  初始化内存管理单元
    mm_init();

    acpi_init();
    // 初始化中断模块
    irq_init();

    // 先初始化系统调用模块
    syscall_init();

    cpu_init();
    //ps2_keyboard_init();
    //ps2_mouse_init();
    ata_init();
    // test_slab();
    // test_mm();

    //  再初始化进程模块。顺序不能调转
    // process_init();
}

//操作系统内核从这里开始执行
void Start_Kernel(void)
{

    system_initialize();

    // show_welcome();
    // test_mm();

/*
    while (1)
    {
        ps2_keyboard_analyze_keycode();
        struct ps2_mouse_packet_3bytes packet = {0};
        // struct ps2_mouse_packet_4bytes packet = {0};
        int errcode = 0;
        errcode = ps2_mouse_get_packet(&packet);
        if (errcode == 0)
        {
            printk_color(GREEN, BLACK, " (Mouse: byte0:%d, x:%3d, y:%3d)\n", packet.byte0, packet.movement_x, packet.movement_y);
            // printk_color(GREEN, BLACK, " (Mouse: byte0:%d, x:%3d, y:%3d, byte3:%3d)\n", packet.byte0, packet.movement_x, packet.movement_y, (unsigned char)packet.byte3);
        }
    }
*/
    /*
        while (1)
        {
            keyboard_analyze_keycode();
            analyze_mousecode();
        }
    */
    while (1)
        ;
}

void ignore_int()
{
    kwarn("Unknown interrupt or fault at RIP.\n");
    return;
}