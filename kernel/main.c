//
// Created by longjin on 2022/1/20.
//

#include "common/glib.h"
#include "common/printk.h"

int *FR_address = (int *)0xffff800000a00000; //帧缓存区的地址

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
    printk_color(0x00e0ebeb, 0x00e0ebeb, "                                \n");
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
    printk("\nTest base 8 : %d --> %o\n", 255, 255);

    //测试输出十六进制
    printk("\nTest base 16 : %d --> %x\n", 255, 255);
    printk("\nTest base 16 : %d --> %X\n", 255, 255);
}
//操作系统内核从这里开始执行
void Start_Kernel(void)
{
    // 初始化printk
    init_printk(1440, 900, FR_address, 1440 * 900 * 4, 8, 16);

    show_welcome();
    test_printk();
    

    while (1)
        ;
}
