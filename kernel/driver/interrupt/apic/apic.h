#pragma once

#include <common/asm.h>
#include <process/ptrace.h>
#include <exception/irq.h>
#include <mm/mm.h>

#pragma GCC push_options
#pragma GCC optimize("O0")


#define APIC_SUCCESS 0
#define APIC_E_NOTFOUND 1

#define APIC_IO_APIC_VIRT_BASE_ADDR SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + IO_APIC_MAPPING_OFFSET
#define APIC_LOCAL_APIC_VIRT_BASE_ADDR SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + LOCAL_APIC_MAPPING_OFFSET

// 当前apic启用状态标志
extern uint8_t __apic_enable_state;
#define APIC_XAPIC_ENABLED 0
#define APIC_X2APIC_ENABLED 1
#define CURRENT_APIC_STATE (__apic_enable_state )

// ======== local apic 寄存器虚拟地址偏移量表 =======
// 0x00~0x10 Reserved.
#define LOCAL_APIC_OFFSET_Local_APIC_ID 0x20
#define LOCAL_APIC_OFFSET_Local_APIC_Version 0x30
// 0x40~0x70 Reserved.
#define LOCAL_APIC_OFFSET_Local_APIC_TPR 0x80
#define LOCAL_APIC_OFFSET_Local_APIC_APR 0x90
#define LOCAL_APIC_OFFSET_Local_APIC_PPR 0xa0
#define LOCAL_APIC_OFFSET_Local_APIC_EOI 0xb0
#define LOCAL_APIC_OFFSET_Local_APIC_RRD 0xc0
#define LOCAL_APIC_OFFSET_Local_APIC_LDR 0xd0
#define LOCAL_APIC_OFFSET_Local_APIC_DFR 0xe0
#define LOCAL_APIC_OFFSET_Local_APIC_SVR 0xf0

#define LOCAL_APIC_OFFSET_Local_APIC_ISR_31_0 0x100
#define LOCAL_APIC_OFFSET_Local_APIC_ISR_63_32 0x110
#define LOCAL_APIC_OFFSET_Local_APIC_ISR_95_64 0x120
#define LOCAL_APIC_OFFSET_Local_APIC_ISR_127_96 0x130
#define LOCAL_APIC_OFFSET_Local_APIC_ISR_159_128 0x140
#define LOCAL_APIC_OFFSET_Local_APIC_ISR_191_160 0x150
#define LOCAL_APIC_OFFSET_Local_APIC_ISR_223_192 0x160
#define LOCAL_APIC_OFFSET_Local_APIC_ISR_255_224 0x170

#define LOCAL_APIC_OFFSET_Local_APIC_TMR_31_0 0x180
#define LOCAL_APIC_OFFSET_Local_APIC_TMR_63_32 0x190
#define LOCAL_APIC_OFFSET_Local_APIC_TMR_95_64 0x1a0
#define LOCAL_APIC_OFFSET_Local_APIC_TMR_127_96 0x1b0
#define LOCAL_APIC_OFFSET_Local_APIC_TMR_159_128 0x1c0
#define LOCAL_APIC_OFFSET_Local_APIC_TMR_191_160 0x1d0
#define LOCAL_APIC_OFFSET_Local_APIC_TMR_223_192 0x1e0
#define LOCAL_APIC_OFFSET_Local_APIC_TMR_255_224 0x1f0

#define LOCAL_APIC_OFFSET_Local_APIC_IRR_31_0 0x200
#define LOCAL_APIC_OFFSET_Local_APIC_IRR_63_32 0x210
#define LOCAL_APIC_OFFSET_Local_APIC_IRR_95_64 0x220
#define LOCAL_APIC_OFFSET_Local_APIC_IRR_127_96 0x230
#define LOCAL_APIC_OFFSET_Local_APIC_IRR_159_128 0x240
#define LOCAL_APIC_OFFSET_Local_APIC_IRR_191_160 0x250
#define LOCAL_APIC_OFFSET_Local_APIC_IRR_223_192 0x260
#define LOCAL_APIC_OFFSET_Local_APIC_IRR_255_224 0x270

#define LOCAL_APIC_OFFSET_Local_APIC_ESR 0x280

// 0x290~0x2e0 Reserved.

#define LOCAL_APIC_OFFSET_Local_APIC_LVT_CMCI 0x2f0
#define LOCAL_APIC_OFFSET_Local_APIC_ICR_31_0 0x300
#define LOCAL_APIC_OFFSET_Local_APIC_ICR_63_32 0x310
#define LOCAL_APIC_OFFSET_Local_APIC_LVT_TIMER 0x320
#define LOCAL_APIC_OFFSET_Local_APIC_LVT_THERMAL 0x330
#define LOCAL_APIC_OFFSET_Local_APIC_LVT_PERFORMANCE_MONITOR 0x340
#define LOCAL_APIC_OFFSET_Local_APIC_LVT_LINT0 0x350
#define LOCAL_APIC_OFFSET_Local_APIC_LVT_LINT1 0x360
#define LOCAL_APIC_OFFSET_Local_APIC_LVT_ERROR 0x370
// 初始计数寄存器（定时器专用）
#define LOCAL_APIC_OFFSET_Local_APIC_INITIAL_COUNT_REG 0x380
// 当前计数寄存器（定时器专用）
#define LOCAL_APIC_OFFSET_Local_APIC_CURRENT_COUNT_REG 0x390
// 0x3A0~0x3D0 Reserved.
// 分频配置寄存器（定时器专用）
#define LOCAL_APIC_OFFSET_Local_APIC_CLKDIV 0x3e0

uint32_t RCBA_vaddr = 0; // RCBA寄存器的虚拟地址

/*

1:	LVT	CMCI
2:	LVT	Timer
3:	LVT	Thermal Monitor
4:	LVT	Performace Counter
5:	LVT	LINT0
6:	LVT	LINT1
7:	LVT	Error

*/
/**
 * LVT表项
 * */
struct apic_LVT
{
    uint vector : 8,         // 0-7位全部置为1
        delivery_mode : 3,   // 第[10:8]位置为100, 表示NMI
        reserved_1 : 1,      // 第11位保留
        delivery_status : 1, // 第12位，投递状态 -> 发送挂起
        polarity : 1,        // 第13位，电平触发极性 存在于LINT0,LINT1
        remote_IRR : 1,      // 第14位，远程IRR标志位（只读） 存在于LINT0,LINT1
        trigger_mode : 1,    // 第15位，触发模式（0位边沿触发，1为电平触发） 存在于LINT0,LINT1
        mask : 1,            // 第16位，屏蔽标志位，（0为未屏蔽， 1为已屏蔽）
        timer_mode : 2,      // 第[18:17]位，定时模式。（00：一次性定时，   01：周期性定时，  10：指定TSC值计数）， 存在于定时器寄存器
        reserved_2 : 13;     // [31:19]位保留

} __attribute((packed)); // 取消结构体的align

/*
    ICR
*/

struct INT_CMD_REG
{
    unsigned int vector : 8, // 0~7
        deliver_mode : 3,    // 8~10
        dest_mode : 1,       // 11
        deliver_status : 1,  // 12
        res_1 : 1,           // 13
        level : 1,           // 14
        trigger : 1,         // 15
        res_2 : 2,           // 16~17
        dest_shorthand : 2,  // 18~19
        res_3 : 12;          // 20~31

    union
    {
        struct
        {
            unsigned int res_4 : 24, // 32~55
                dest_field : 8;      // 56~63
        } apic_destination;

        unsigned int x2apic_destination; // 32~63
    } destination;

} __attribute__((packed));

/**
 * @brief I/O APIC 的中断定向寄存器的结构体
 *
 */
struct apic_IO_APIC_RTE_entry
{
    unsigned int vector : 8, // 0~7
        deliver_mode : 3,    // [10:8] 投递模式默认为NMI
        dest_mode : 1,       // 11 目标模式(0位物理模式，1为逻辑模式)
        deliver_status : 1,  // 12 投递状态
        polarity : 1,        // 13 电平触发极性
        remote_IRR : 1,      // 14 远程IRR标志位（只读）
        trigger_mode : 1,    // 15 触发模式（0位边沿触发，1为电平触发）
        mask : 1,            // 16 屏蔽标志位，（0为未屏蔽， 1为已屏蔽）
        reserved : 15;       // [31:17]位保留

    union
    {
        // 物理模式
        struct
        {
            unsigned int reserved1 : 24, // [55:32] 保留
                phy_dest : 4,            // [59:56] APIC ID
                reserved2 : 4;           // [63:60] 保留
        } physical;

        // 逻辑模式
        struct
        {
            unsigned int reserved1 : 24, // [55:32] 保留
                logical_dest : 8;        // [63:56] 自定义APIC ID
        } logical;
    } destination;
} __attribute__((packed));

// ========== APIC的寄存器的参数定义 ==============
// 投递模式
#define LOCAL_APIC_FIXED 0
#define IO_APIC_FIXED 0
#define ICR_APIC_FIXED 0

#define IO_APIC_Lowest_Priority 1
#define ICR_Lowest_Priority 1

#define LOCAL_APIC_SMI 2
#define APIC_SMI 2
#define ICR_SMI 2

#define LOCAL_APIC_NMI 4
#define APIC_NMI 4
#define ICR_NMI 4

#define LOCAL_APIC_INIT 5
#define APIC_INIT 5
#define ICR_INIT 5

#define ICR_Start_up 6

#define IO_APIC_ExtINT 7

// 时钟模式
#define APIC_LVT_Timer_One_Shot 0
#define APIC_LVT_Timer_Periodic 1
#define APIC_LVT_Timer_TSC_Deadline 2

// 屏蔽
#define UNMASKED 0
#define MASKED 1
#define APIC_LVT_INT_MASKED 0x10000UL

// 触发模式
#define EDGE_TRIGGER 0  // 边沿触发
#define Level_TRIGGER 1 // 电平触发

// 投递模式
#define IDLE 0         // 挂起
#define SEND_PENDING 1 // 发送等待

// destination shorthand
#define ICR_No_Shorthand 0
#define ICR_Self 1
#define ICR_ALL_INCLUDE_Self 2
#define ICR_ALL_EXCLUDE_Self 3

// 投递目标模式
#define DEST_PHYSICAL 0 // 物理模式
#define DEST_LOGIC 1    // 逻辑模式

// level
#define ICR_LEVEL_DE_ASSERT 0
#define ICR_LEVEL_ASSERT 1

// 远程IRR标志位, 在处理Local APIC标志位时置位，在收到处理器发来的EOI命令时复位
#define IRR_RESET 0
#define IRR_ACCEPT 1

// 电平触发极性
#define POLARITY_HIGH 0
#define POLARITY_LOW 1

struct apic_IO_APIC_map
{
    // 间接访问寄存器的物理基地址
    uint addr_phys;
    // 索引寄存器虚拟地址
    unsigned char *virtual_index_addr;
    // 数据寄存器虚拟地址
    uint *virtual_data_addr;
    // EOI寄存器虚拟地址
    uint *virtual_EOI_addr;
} apic_ioapic_map;

/**
 * @brief 中断服务程序
 *
 * @param rsp 中断栈指针
 * @param number 中断向量号
 */
void do_IRQ(struct pt_regs *rsp, ul number);

/**
 * @brief 读取RTE寄存器
 *
 * @param index 索引值
 * @return ul
 */
ul apic_ioapic_read_rte(unsigned char index);

/**
 * @brief 写入RTE寄存器
 *
 * @param index 索引值
 * @param value 要写入的值
 */
void apic_ioapic_write_rte(unsigned char index, ul value);

/**
 * @brief 初始化AP处理器的Local apic
 *
 */
void apic_init_ap_core_local_apic();

/**
 * @brief 初始化apic控制器
 *
 */
int apic_init();

/**
 * @brief 读取指定类型的 Interrupt Control Structure
 *
 * @param type ics的类型
 * @param ret_vaddr 对应的ICS的虚拟地址数组
 * @param total 返回数组的元素总个数
 * @return uint
 */
uint apic_get_ics(const uint type, ul ret_vaddr[], uint *total);

// =========== 中断控制操作接口 ============
void apic_ioapic_enable(ul irq_num);
void apic_ioapic_disable(ul irq_num);
ul apic_ioapic_install(ul irq_num, void *arg);
void apic_ioapic_uninstall(ul irq_num);
void apic_ioapic_level_ack(ul irq_num); // ioapic电平触发 应答
void apic_ioapic_edge_ack(ul irq_num);  // ioapic边沿触发 应答

// void apic_local_apic_level_ack(ul irq_num);// local apic电平触发 应答
void apic_local_apic_edge_ack(ul irq_num); // local apic边沿触发 应答

/**
 * @brief 构造RTE Entry结构体
 *
 * @param entry 返回的结构体
 * @param vector 中断向量
 * @param deliver_mode 投递模式
 * @param dest_mode 目标模式
 * @param deliver_status 投递状态
 * @param polarity 电平触发极性
 * @param irr 远程IRR标志位（只读）
 * @param trigger 触发模式
 * @param mask 屏蔽标志位，（0为未屏蔽， 1为已屏蔽）
 * @param dest_apicID 目标apicID
 */
void apic_make_rte_entry(struct apic_IO_APIC_RTE_entry *entry, uint8_t vector, uint8_t deliver_mode, uint8_t dest_mode,
                         uint8_t deliver_status, uint8_t polarity, uint8_t irr, uint8_t trigger, uint8_t mask, uint8_t dest_apicID);

#pragma GCC pop_options