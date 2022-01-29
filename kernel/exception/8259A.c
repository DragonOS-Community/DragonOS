#include "8259A.h"
#include "irq.c"
#include "../common/printk.h"
#include "../common/kprint.h"

void init_8259A()
{
    // 初始化中断门， 中断使用第二个ist
    for(int i=32;i<=55;++i)
        set_intr_gate(i, 2, interrupt[i-32]);
    kinfo("Initializing 8259A...");
    
    // 初始化主芯片
    io_out8(0x20, 0x11);    // 初始化主芯片的icw1
    io_out8(0x21, 0x20);    // 设置主芯片的中断向量号为0x20(0x20-0x27)
    io_out8(0x21, 0x04);    // 设置int2端口级联从芯片
    io_out8(0x21, 0x01);    // 设置为AEOI模式、FNM、无缓冲

    // 初始化从芯片
    io_out8(0xa0, 0x11);
    io_out8(0xa1, 0x28);    // 设置从芯片的中断向量号为0x28(0x28-0x2f)
    io_out8(0xa1, 0x02);    // 设置从芯片连接到主芯片的int2
    io_out8(0xa1, 0x01);


    // 设置ocw1, 允许所有中断请求
    io_out8(0x21, 0x00);
    io_out8(0xa1, 0x00);

    sti();

    kinfo("IRQ circuit 8259A initialized.");

}

