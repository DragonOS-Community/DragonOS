#include "irq.h"
#include "8259A.h"
#include "../common/asm.h"
#include"../common/printk.h"
#include "gate.h"

// 保存函数调用现场的寄存器
#define SAVE_ALL_REGS      \
    "cld; \n\t"             \
    "pushq %rax;    \n\t"   \
    "pushq %rax;     \n\t"  \
    "movq %es, %rax; \n\t"  \
    "pushq %rax;     \n\t"  \
    "movq %ds, %rax; \n\t"  \
    "pushq %rax;     \n\t"  \
    "xorq %rax, %rax;\n\t"  \
    "pushq %rbp;     \n\t"  \
    "pushq %rdi;     \n\t"  \
    "pushq %rsi;     \n\t"  \
    "pushq %rdx;     \n\t"  \
    "pushq %rcx;     \n\t"  \
    "pushq %rbx;     \n\t"  \
    "pushq %r8 ;    \n\t"   \
    "pushq %r9 ;    \n\t"   \
    "pushq %r10;     \n\t"  \
    "pushq %r11;     \n\t"  \
    "pushq %r12;     \n\t"  \
    "pushq %r13;     \n\t"  \
    "pushq %r14;     \n\t"  \
    "pushq %r15;     \n\t"  \
    "movq $0x10, %rdx;\n\t" \
    "movq %rdx, %ds; \n\t"  \
    "movq %rdx, %es; \n\t"


// 定义IRQ处理函数的名字格式：IRQ+中断号+interrupt
#define IRQ_NAME2(name1) name1##interrupt(void)
#define IRQ_NAME(number) IRQ_NAME2(IRQ##number)

// 构造中断entry
// 为了复用返回函数的代码，需要压入一个错误码0

#define Build_IRQ(number)                                                     \
    void IRQ_NAME(number);                                                    \
    __asm__ (SYMBOL_NAME_STR(IRQ)#number"interrupt:   \n\t"                 \
                                         "pushq $0x00 \n\t" \
                                         SAVE_ALL_REGS     \
                                         "movq %rsp, %rdi\n\t"                \
                                         "leaq ret_from_intr(%rip), %rax\n\t" \
                                         "pushq %rax \n\t"                    \
                                         "movq	$"#number",	%rsi			\n\t"	\
                                         "jmp do_IRQ\n\t");
                                         


// 构造中断入口
Build_IRQ(0x20)
Build_IRQ(0x21)
Build_IRQ(0x22)
Build_IRQ(0x23)
Build_IRQ(0x24)
Build_IRQ(0x25)
Build_IRQ(0x26)
Build_IRQ(0x27)
Build_IRQ(0x28)
Build_IRQ(0x29)
Build_IRQ(0x2a)
Build_IRQ(0x2b)
Build_IRQ(0x2c)
Build_IRQ(0x2d)
Build_IRQ(0x2e)
Build_IRQ(0x2f)
Build_IRQ(0x30)
Build_IRQ(0x31)
Build_IRQ(0x32)
Build_IRQ(0x33)
Build_IRQ(0x34)
Build_IRQ(0x35)
Build_IRQ(0x36)
Build_IRQ(0x37)

// 初始化中断数组
void (*interrupt[24])(void)=
{
    IRQ0x20interrupt,
    IRQ0x21interrupt,
    IRQ0x22interrupt,
    IRQ0x23interrupt,
    IRQ0x24interrupt,
    IRQ0x25interrupt,
    IRQ0x26interrupt,
    IRQ0x27interrupt,
    IRQ0x28interrupt,
    IRQ0x29interrupt,
    IRQ0x2ainterrupt,
    IRQ0x2binterrupt,
    IRQ0x2cinterrupt,
    IRQ0x2dinterrupt,
    IRQ0x2einterrupt,
    IRQ0x2finterrupt,
    IRQ0x30interrupt,
    IRQ0x31interrupt,
    IRQ0x32interrupt,
    IRQ0x33interrupt,
    IRQ0x34interrupt,
    IRQ0x35interrupt,
    IRQ0x36interrupt,
    IRQ0x37interrupt,
};


/**
 * @brief 初始化中断模块
 */
void init_irq()
{
    init_8259A();
}


/**
 * @brief 中断服务程序
 * 
 * @param rsp 中断栈指针
 * @param number 中断号
 */
void do_IRQ(struct pt_regs *regs, ul number)
{
    unsigned char x;
    switch (number)
    {
    case 0x20:  // 时钟中断信号
        
        break;
    case 0x21:  // 键盘中断
        
        x = io_in8(0x60);
        printk_color(ORANGE, BLACK, "Received key irq, key code:%#018lx\n", x);
        break;
    default:
        break;
    }
    if(number!=0x20)
    printk_color(ORANGE, BLACK, "Received irq:%#018x\n", number);

    // 向主芯片发送中断结束信号
    io_out8(PIC_master, PIC_EOI);
}