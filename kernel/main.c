//
// Created by longjin on 2022/1/20.
//

#include "common/glib.h"
#include "common/printk.h"
#include "exception/gate.h"
#include "exception/trap.h"
#include "exception/irq.h"
#include "mm/mm.h"
#include "common/kprint.h"

int *FR_address = (int *)0xffff800000a00000; //帧缓存区的地址
//char fxsave_region[512] __attribute__((aligned(16)));

struct memory_desc memory_management_struct = {{0}, 0};

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

void test_printk()
{
    //测试直接输出
    printk("\nTesting printk...\n");
    //测试输出单个字符
    printk("%c\n", 't');
    //测试输出字符串%s
    printk("%s\n", "xxx");

    //测试输出数字
    printk("%d %ld %lld\n", 1, 2, 3);

    //测试输出两个百分号
    printk("%%\n");

    //测试输出\t
    printk("\nTesting tab...\n");
    printk("date\t\tname\tscore\n");
    printk("2022-01-01\tDavid\t99\n");
    printk("2022-01-01\tJohn\t95\n");

    //测试输出八进制
    printk("\nTest base 8 : %d --> %#o\n", 255, 255);

    //测试输出十六进制
    printk("\nTest base 16 : %d --> %#x\n", 255, 255);
    printk("\nTest base 16 : %d --> %#X\n", 255, 255);
}

// 测试内存管理单元
void test_mm()
{
    kinfo("Testing memory management unit...");
    //printk("bmp[0]:%#018x\tbmp[1]%#018lx\n", *memory_management_struct.bmp, *(memory_management_struct.bmp + 1));
    kinfo("Try to allocate 64 memory pages.");
    struct Page *page = alloc_pages(ZONE_NORMAL, 64, PAGE_PGT_MAPPED | PAGE_ACTIVE | PAGE_KERNEL);
    /*
    for (int i = 0; i <= 65; ++i)
    {
        printk("page%d\tattr:%#018lx\tphys_addr:%#018lx\t", i, page->attr, page->addr_phys);
        ++page;
        if (((i + 1) % 2) == 0)
            printk("\n");
    }
    */
    
   //printk("bmp[0]:%#018x\tbmp[1]%#018lx\n", *(memory_management_struct.bmp), *(memory_management_struct.bmp + 1));
}

void init()
{
    // 初始化printk
    init_printk(1440, 900, FR_address, 1440 * 900 * 4, 8, 16);

    load_TR(8); // 加载TR寄存器

    // 初始化任务状态段表
    ul tss_item_addr = 0xffff800000007c00;
    set_TSS64(tss_item_addr, tss_item_addr, tss_item_addr, tss_item_addr, tss_item_addr,
              tss_item_addr, tss_item_addr, tss_item_addr, tss_item_addr, tss_item_addr);

    // 初始化中断描述符表
    init_sys_vector();

    //asm volatile(" fxsave %0 " ::"m"(fxsave_region));
    // 初始化内存管理单元
    mm_init();

    // 初始化中断模块
    init_irq();


}
//操作系统内核从这里开始执行
void Start_Kernel(void)
{

    init();
    //show_welcome();
    test_mm();

    //test_printk();

    //int t = 1 / 0; // 测试异常处理模块能否正常工作 触发除法错误
    // int t = *(int *)0xffff80000aa00000; // 触发页故障

    while (1)
        ;
}

void ignore_int()
{
    printk("[");
    printk_color(YELLOW, BLACK, "WARN");
    printk("] Unknown interrupt or fault at RIP.\n");
    return;
}
