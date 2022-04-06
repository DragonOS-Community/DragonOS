#include "irq.h"

// 对进行
#if _INTR_8259A_
#include "../driver/interrupt/8259A/8259A.h"
#else
#include "../driver/interrupt/apic/apic.h"
#endif

#include "../common/asm.h"
#include "../common/printk.h"
#include "gate.h"

// 保存函数调用现场的寄存器
#define SAVE_ALL_REGS       \
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
    __asm__(SYMBOL_NAME_STR(IRQ) #number "interrupt:   \n\t"                  \
                                         "pushq $0x00 \n\t" \
                                         SAVE_ALL_REGS     \
                                         "movq %rsp, %rdi   \n\t"                \
                                         "leaq ret_from_intr(%rip), %rax    \n\t" \
                                         "pushq %rax \n\t"                    \
                                         "movq	$"#number",	%rsi			\n\t"    \
                                         "jmp do_IRQ    \n\t");

// 构造中断入口
Build_IRQ(0x20);
Build_IRQ(0x21);
Build_IRQ(0x22);
Build_IRQ(0x23);
Build_IRQ(0x24);
Build_IRQ(0x25);
Build_IRQ(0x26);
Build_IRQ(0x27);
Build_IRQ(0x28);
Build_IRQ(0x29);
Build_IRQ(0x2a);
Build_IRQ(0x2b);
Build_IRQ(0x2c);
Build_IRQ(0x2d);
Build_IRQ(0x2e);
Build_IRQ(0x2f);
Build_IRQ(0x30);
Build_IRQ(0x31);
Build_IRQ(0x32);
Build_IRQ(0x33);
Build_IRQ(0x34);
Build_IRQ(0x35);
Build_IRQ(0x36);
Build_IRQ(0x37);

// 初始化中断数组
void (*interrupt_table[24])(void) =
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
 * @brief 中断注册函数
 *
 * @param irq_num 中断向量号
 * @param arg 传递给中断安装接口的参数
 * @param handler 中断处理函数
 * @param paramater 中断处理函数的参数
 * @param controller 中断控制器结构
 * @param irq_name 中断名
 * @return int
 */
int irq_register(ul irq_num, void *arg, void (*handler)(ul irq_num, ul parameter, struct pt_regs *regs), ul paramater, hardware_intr_controller *controller, char *irq_name)
{
    // 由于为I/O APIC分配的中断向量号是从32开始的，因此要减去32才是对应的interrupt_desc的元素
    irq_desc_t *p = &interrupt_desc[irq_num - 32];

    p->controller = controller;
    p->irq_name = irq_name;
    p->parameter = paramater;
    p->flags = 0;
    p->handler = handler;

    p->controller->install(irq_num, arg);
    p->controller->enable(irq_num);

    return 0;
}

/**
 * @brief 中断注销函数
 *
 * @param irq_num 中断向量号
 * @return int
 */
int irq_unregister(ul irq_num)
{
    irq_desc_t *p = &interrupt_desc[irq_num - 32];
    p->controller->disable(irq_num);
    p->controller->uninstall(irq_num);

    p->controller = NULL;
    p->irq_name = NULL;
    p->parameter = NULL;
    p->flags = 0;
    p->handler = NULL;
    
    return 0;
}

/**
 * @brief 初始化中断模块
 */
void irq_init()
{
#if _INTR_8259A_
    init_8259A();
#else

    apic_init();
    memset(interrupt_desc, 0, sizeof(irq_desc_t) * IRQ_NUM);
#endif
}
