/**
 * @file irq.h
 * @author longjin
 * @brief 中断处理程序
 * @version 0.1
 * @date 2022-01-28
 *
 * @copyright Copyright (c) 2022
 *
 */

#pragma once
#include <common/glib.h>

#include <process/ptrace.h>
#pragma GCC push_options
#pragma GCC optimize ("O0")

#define IRQ_NUM 24
#define SMP_IRQ_NUM 10
#define LOCAL_APIC_IRQ_NUM 50

extern void (*interrupt_table[24])(void);
extern void do_IRQ(struct pt_regs *regs, ul number);


extern void (*SMP_interrupt_table[SMP_IRQ_NUM])(void);

extern void (*syscall_intr_table[1])(void);
extern void (*local_apic_interrupt_table[LOCAL_APIC_IRQ_NUM])(void);

/* ========= 中断向量分配表 ==========

0~255 IDT

0   ~   31	trap fault abort for system
	0	devide error
	1	debug
	2	NMI
	3	breakpoint
	4	overflow
	5	bound range
	6	undefined opcode
	7	device	not available
	8	double fault
	9	coprocessor segment overrun
	10	invalid TSS
	11	segment not present
	12	stack segment fault
	13	general protection
	14	page fault
	15	
	16	x87 FPU error
	17	alignment check
	18	machine check
	19	SIMD exception
	20	virtualization exception
21  ~   31	Do not use

32  ~   55	I/O APIC
	32	8259A
	33	keyboard
	34	HPET timer 0,8254 counter 0
	35	serial port A
	36	serial port B
	37	parallel port
	38	floppy
	39	parallel port
	40	RTC,HPET timer 1
	41	Generic
	42	Generic
	43	HPET timer 2
	44	HPET timer 3	/ mouse
	45	FERR#
	46	SATA primary
	47	SATA secondary
	48	PIRQA
	49	PIRQB
	50	PIRQC
	51	PIRQD
	52	PIRQE
	53	PIRQF
	54	PIRQG
	55	PIRQH
	
	
0x80		system call
0x81		system interrupt 系统中断

[150,200)	Local APIC
	150	CMCI
	151	Timer
	152	Thermal Monitor
	153	Performance Counter
	154	LINT0
	155	LINT1
	156	Error
	157 xhci_controller_0
	158 xhci_controller_1
	159 xhci_controller_2
	160 xhci_controller_3

200 ~   255	MP IPI

*/

typedef struct hardware_intr_type
{
    // 使能中断操作接口
    void (*enable)(ul irq_num);
    // 禁止中断操作接口
    void (*disable)(ul irq_num);

    // 安装中断操作接口
    ul (*install)(ul irq_num, void *arg);
    // 卸载中断操作接口
    void (*uninstall)(ul irq_num);
    // 应答中断操作接口
    void (*ack)(ul irq_num);
} hardware_intr_controller;

// 中断描述结构体
typedef struct
{
    hardware_intr_controller *controller;
    // 中断名
    char *irq_name;
    // 中断处理函数的参数
    ul parameter;
    // 中断处理函数
    void (*handler)(ul irq_num, ul parameter, struct pt_regs *regs);

    // 自定义的标志位
    ul flags;
} irq_desc_t;


// 这几个表一定要放在这里，否则在HPET初始化后收到中断，会产生page fault
irq_desc_t interrupt_desc[IRQ_NUM] = {0};
irq_desc_t local_apic_interrupt_desc[LOCAL_APIC_IRQ_NUM] = {0};
irq_desc_t SMP_IPI_desc[SMP_IRQ_NUM] = {0};


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
int irq_register(ul irq_num, void *arg, void (*handler)(ul irq_num, ul parameter, struct pt_regs *regs), ul paramater, hardware_intr_controller *controller, char *irq_name);

/**
 * @brief 中断注销函数
 * 
 * @param irq_num 中断向量号
 * @return int 
 */
int irq_unregister(ul irq_num);

/**
 * @brief 初始化中断模块
 */
void irq_init();
#pragma GCC pop_options