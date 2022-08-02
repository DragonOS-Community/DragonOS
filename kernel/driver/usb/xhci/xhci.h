#pragma once
#include <driver/usb/usb.h>
#include <driver/pci/pci.h>
#include <driver/pci/msi.h>
// #pragma GCC optimize("O0")
#define XHCI_MAX_HOST_CONTROLLERS 4 // 本驱动程序最大支持4个xhci root hub controller
#define XHCI_MAX_ROOT_HUB_PORTS 128 // 本驱动程序最大支持127个root hub 端口（第0个保留）

// ========== irq BEGIN ===========
/**
 * @brief 每个xhci控制器的中断向量号
 *
 */
const uint8_t xhci_controller_irq_num[XHCI_MAX_HOST_CONTROLLERS] = {157, 158, 159, 160};

/**
 * @brief 通过irq号寻找对应的主机控制器id
 *
 */
#define xhci_find_hcid_by_irq_num(irq_num) ({           \
    int retval = -1;                                    \
    for (int i = 0; i < XHCI_MAX_HOST_CONTROLLERS; ++i) \
        if (xhci_controller_irq_num[i] == irq_num)      \
            retval = i;                                 \
    retval;                                             \
})

struct xhci_hc_irq_install_info_t
{
    int processor;       // 中断目标处理器
    int8_t edge_trigger; // 是否边缘触发
    int8_t assert;       // 是否高电平触发
};
// ========== irq END ===========

// ======== Capability Register Set BEGIN ============

// xhci Capability Registers offset
#define XHCI_CAPS_CAPLENGTH 0x00 // Cap 寄存器组的长度
#define XHCI_CAPS_RESERVED 0x01
#define XHCI_CAPS_HCIVERSION 0x02 // 接口版本号
#define XHCI_CAPS_HCSPARAMS1 0x04
#define XHCI_CAPS_HCSPARAMS2 0x08
#define XHCI_CAPS_HCSPARAMS3 0x0c
#define XHCI_CAPS_HCCPARAMS1 0x10 // capability params 1
#define XHCI_CAPS_DBOFF 0x14      // Doorbell offset
#define XHCI_CAPS_RTSOFF 0x18     // Runtime register space offset
#define XHCI_CAPS_HCCPARAMS2 0x1c // capability params 2

struct xhci_caps_HCSPARAMS1_reg_t
{
    unsigned max_slots : 8;  // 最大插槽数
    unsigned max_intrs : 11; // 最大中断数
    unsigned reserved : 5;
    unsigned max_ports : 8; // 最大端口数
} __attribute__((packed));

struct xhci_caps_HCSPARAMS2_reg_t
{
    unsigned ist : 4;      // 同步调度阈值
    unsigned ERST_Max : 4; // Event Ring Segment Table: Max segs
    unsigned Reserved : 13;
    unsigned max_scratchpad_buf_HI5 : 5; // 草稿行buffer地址（高5bit）
    unsigned spr : 1;                    // scratchpad restore
    unsigned max_scratchpad_buf_LO5 : 5; // 草稿行buffer地址（低5bit）
} __attribute__((packed));

struct xhci_caps_HCSPARAMS3_reg_t
{
    uint8_t u1_device_exit_latency; // 0~10ms
    uint8_t Reserved;
    uint16_t u2_device_exit_latency; // 0~2047ms
} __attribute__((packed));

struct xhci_caps_HCCPARAMS1_reg_t
{
    unsigned int ac64 : 1; // 64-bit addressing capability
    unsigned int bnc : 1;  // bw negotiation capability
    unsigned int csz : 1;  // context size
    unsigned int ppc : 1;  // 端口电源控制
    unsigned int pind : 1; // port indicators
    unsigned int lhrc : 1; // Light HC reset capability
    unsigned int ltc : 1;  // latency tolerance messaging capability
    unsigned int nss : 1;  // no secondary SID support

    unsigned int pae : 1;        // parse all event data
    unsigned int spc : 1;        // Stopped - Short packet capability
    unsigned int sec : 1;        // Stopped EDTLA capability
    unsigned int cfc : 1;        // Continuous Frame ID capability
    unsigned int MaxPSASize : 4; // Max Primary Stream Array Size

    uint16_t xECP; // xhci extended capabilities pointer

} __attribute__((packed));

struct xhci_caps_HCCPARAMS2_reg_t
{
    unsigned u3c : 1; // U3 Entry Capability
    unsigned cmc : 1; // ConfigEP command Max exit latency too large
    unsigned fsc : 1; // Force Save Context Capability
    unsigned ctc : 1; // Compliance Transition Capability
    unsigned lec : 1; // large ESIT payload capability
    unsigned cic : 1; // configuration information capability
    unsigned Reserved : 26;
} __attribute__((packed));
// ======== Capability Register Set END ============

// ======== Operational Register Set BEGIN =========

// xhci operational registers offset
#define XHCI_OPS_USBCMD 0x00   // USB Command
#define XHCI_OPS_USBSTS 0x04   // USB status
#define XHCI_OPS_PAGESIZE 0x08 // Page size
#define XHCI_OPS_DNCTRL 0x14   // Device notification control
#define XHCI_OPS_CRCR 0x18     // Command ring control
#define XHCI_OPS_DCBAAP 0x30   // Device context base address array pointer
#define XHCI_OPS_CONFIG 0x38   // configuire
#define XHCI_OPS_PRS 0x400     // Port register sets

struct xhci_ops_usbcmd_reg_t
{
    unsigned rs : 1;          // Run/Stop
    unsigned hcrst : 1;       // host controller reset
    unsigned inte : 1;        // Interrupt enable
    unsigned hsee : 1;        // Host system error enable
    unsigned rsvd_psvd1 : 3;  // Reserved and preserved
    unsigned lhcrst : 1;      // light host controller reset
    unsigned css : 1;         // controller save state
    unsigned crs : 1;         // controller restore state
    unsigned ewe : 1;         // enable wrap event
    unsigned ue3s : 1;        // enable U3 MFINDEX Stop
    unsigned spe : 1;         // stopped short packet enable
    unsigned cme : 1;         // CEM Enable
    unsigned rsvd_psvd2 : 18; // Reserved and preserved
} __attribute__((packed));

struct xhci_ops_usbsts_reg_t
{
    unsigned HCHalted : 1;
    unsigned rsvd_psvd1 : 1;  // Reserved and preserved
    unsigned hse : 1;         // Host system error
    unsigned eint : 1;        // event interrupt
    unsigned pcd : 1;         // Port change detected
    unsigned rsvd_zerod : 3;  // Reserved and Zero'd
    unsigned sss : 1;         // Save State Status
    unsigned rss : 1;         // restore state status
    unsigned sre : 1;         // save/restore error
    unsigned cnr : 1;         // controller not ready
    unsigned hce : 1;         // host controller error
    unsigned rsvd_psvd2 : 19; // Reserved and Preserved
} __attribute__((packed));

struct xhci_ops_pagesize_reg_t
{
    uint16_t page_size; // The actual pagesize is ((this field)<<12)
    uint16_t reserved;
} __attribute__((packed));

struct xhci_ops_dnctrl_reg_t
{
    uint16_t value;
    uint16_t reserved;
} __attribute__((packed));

struct xhci_ops_config_reg_t
{
    uint8_t MaxSlotsEn;      // Max slots enabled
    unsigned u3e : 1;        // U3 Entry Enable
    unsigned cie : 1;        // Configuration information enable
    unsigned rsvd_psvd : 22; // Reserved and Preserved
} __attribute__((packed));

// ======== Operational Register Set END =========
// ========= TRB begin ===========

// TRB的Transfer Type可用值定义
#define XHCI_TRB_TRT_NO_DATA 0
#define XHCI_TRB_TRT_RESERVED 1
#define XHCI_TRB_TRT_OUT_DATA 2
#define XHCI_TRB_TRT_IN_DATA 3

#define XHCI_CMND_RING_TRBS 128 // TRB num of command ring,  not more than 4096

#define XHCI_TRBS_PER_RING 256

#define XHCI_TRB_CYCLE_OFF 0
#define XHCI_TRB_CYCLE_ON 1

/**
 * @brief xhci通用TRB结构
 *
 */
struct xhci_TRB_t
{
    uint64_t param; // 参数
    uint32_t status;
    uint32_t command;
} __attribute__((packed));
struct xhci_TRB_normal_t
{
    uint64_t buf_paddr; // 数据缓冲区物理地址

    unsigned transfer_length : 17; // 传输数据长度
    unsigned TD_size : 5;          // 传输描述符中剩余的数据包的数量
    unsigned intr_target : 10;     // 中断目标 [0:MaxIntrs-1]

    unsigned cycle : 1;    // used to mark the enqueue pointer of transfer ring
    unsigned ent : 1;      // evaluate next TRB before updating the endpoint's state
    unsigned isp : 1;      // Interrupt on short packet bit
    unsigned ns : 1;       // No snoop
    unsigned chain : 1;    // The chain bit is used to tell the controller that this
                           // TRB is associated with the next TRB in the TD
    unsigned ioc : 1;      // 完成时发起中断
    unsigned idt : 1;      // Immediate Data
    unsigned resv : 2;     // Reserved and zero'd
    unsigned bei : 1;      // Block event interrupt
    unsigned TRB_type : 6; // TRB类型
    uint16_t Reserved;     // 保留且置为0
} __attribute__((packed));

struct xhci_TRB_setup_state_t
{
    uint8_t bmRequestType;
    uint8_t bRequest;
    uint16_t wValue;

    uint16_t wIndex;
    uint16_t wLength;

    unsigned transfer_legth : 17;
    unsigned resv1 : 5; // Reserved and zero'd
    unsigned intr_target : 10;

    unsigned cycle : 1;
    unsigned resv2 : 4; // Reserved and zero'd
    unsigned ioc : 1;
    unsigned idt : 1;
    unsigned resv3 : 3; // Reserved and zero'd
    unsigned TRB_type : 6;
    unsigned trt : 2;    // Transfer type
    unsigned resv4 : 14; // Reserved and zero'd

} __attribute__((packed));

struct xhci_TRB_data_stage_t
{
    uint64_t buf_paddr; // 数据缓冲区物理地址

    unsigned transfer_length : 17; // 传输数据长度
    unsigned TD_size : 5;          // 传输描述符中剩余的数据包的数量
    unsigned intr_target : 10;     // 中断目标 [0:MaxIntrs-1]

    unsigned cycle : 1;     // used to mark the enqueue pointer of transfer ring
    unsigned ent : 1;       // evaluate next TRB before updating the endpoint's state
    unsigned isp : 1;       // Interrupt on short packet bit
    unsigned ns : 1;        // No snoop
    unsigned chain : 1;     // The chain bit is used to tell the controller that this
                            // TRB is associated with the next TRB in the TD
    unsigned ioc : 1;       // 完成时发起中断
    unsigned idt : 1;       // Immediate Data
    unsigned resv : 3;      // Reserved and zero'd
    unsigned TRB_type : 6;  // TRB类型
    unsigned dir : 1;       // 0 -> out packet
                            // 1 -> in packet
    unsigned Reserved : 15; // 保留且置为0
} __attribute__((packed));

struct xhci_TRB_status_stage_t
{
    uint64_t resv1; // Reserved and zero'd

    unsigned resv2 : 22;       // Reserved and zero'd
    unsigned intr_target : 10; // 中断目标 [0:MaxIntrs-1]

    unsigned cycle : 1;     // used to mark the enqueue pointer of transfer ring
    unsigned ent : 1;       // evaluate next TRB before updating the endpoint's state
    unsigned resv3 : 2;     // Reserved and zero'd
    unsigned chain : 1;     // The chain bit is used to tell the controller that this
                            // TRB is associated with the next TRB in the TD
    unsigned ioc : 1;       // 完成时发起中断
    unsigned resv4 : 4;     // Reserved and zero'd
    unsigned TRB_type : 6;  // TRB类型
    unsigned dir : 1;       // 0 -> out packet
                            // 1 -> in packet
    unsigned Reserved : 15; // 保留且置为0
} __attribute__((packed));

struct xhci_TRB_cmd_complete_t
{
    uint64_t cmd_trb_pointer_paddr; //  指向生成当前Event TRB的TRB的物理地址（16bytes对齐）

    unsigned resv1 : 24; // Reserved and zero'd
    uint8_t code;        // Completion code

    unsigned cycle : 1;    // cycle bit
    unsigned resv2 : 9;    // Reserved and zero'd
    unsigned TRB_type : 6; // TRB类型
    uint8_t VF_ID;
    uint8_t slot_id; // the id of the slot associated with the
                     // command that generated the event
} __attribute__((packed));
// ========= TRB end ===========

// ======== Runtime Register Set Begin =========

#define XHCI_RT_IR0 0x20 // 中断寄存器组0距离runtime Register set起始位置的偏移量
#define XHCI_IR_SIZE 32  // 中断寄存器组大小

// 中断寄存器组内的偏移量
#define XHCI_IR_MAN 0x00        // Interrupter Management Register
#define XHCI_IR_MOD 0x04        // Interrupter Moderation
#define XHCI_IR_TABLE_SIZE 0x08 // Event Ring Segment Table size (count of segments)
#define XHCI_IR_TABLE_ADDR 0x10 // Event Ring Segment Table Base Address
#define XHCI_IR_DEQUEUE 0x18    // Event Ring Dequeue Pointer

// MAN寄存器内的bit的含义
#define XHCI_IR_IMR_PENDING (1 << 0) // Interrupt pending bit in Management Register
#define XHCI_IR_IMR_ENABLE (1 << 1)  // Interrupt enable bit in Management Register

struct xhci_intr_moderation_t
{
    uint16_t interval; // 产生一个中断的时间，是interval*250ns (wait before next interrupt)
    uint16_t counter;
} __attribute__((packed));
// ======== Runtime Register Set END =========

// ======= xhci Extended Capabilities List BEGIN========

// ID 部分的含义定义
#define XHCI_XECP_ID_RESERVED 0
#define XHCI_XECP_ID_LEGACY 1    // USB Legacy Support
#define XHCI_XECP_ID_PROTOCOL 2  // Supported protocol
#define XHCI_XECP_ID_POWER 3     // Extended power management
#define XHCI_XECP_ID_IOVIRT 4    // I/0 virtualization
#define XHCI_XECP_ID_MSG 5       // Message interrupt
#define XHCI_XECP_ID_LOCAL_MEM 6 // local memory
#define XHCI_XECP_ID_DEBUG 10    // USB Debug capability
#define XHCI_XECP_ID_EXTMSG 17   // Extended message interrupt

#define XHCI_XECP_LEGACY_TIMEOUT 10           // 设置legacy状态的等待时间
#define XHCI_XECP_LEGACY_BIOS_OWNED (1 << 16) // 当bios控制着该hc时，该位被置位
#define XHCI_XECP_LEGACY_OS_OWNED (1 << 24)   // 当系统控制着该hc时，该位被置位
#define XHCI_XECP_LEGACY_OWNING_MASK (XHCI_XECP_LEGACY_BIOS_OWNED | XHCI_XECP_LEGACY_OS_OWNED)

// ======= xhci Extended Capabilities List END ========

// ======= Port status and control registers BEGIN ====
#define XHCI_PORT_PORTSC 0x00    // Port status and control
#define XHCI_PORT_PORTPMSC 0x04  // Port power management status and control
#define XHCI_PORT_PORTLI 0x08    // Port Link info
#define XHCI_PORT_PORTHLMPC 0x0c // Port hardware LPM control (version 1.10 only

#define XHCI_PORTUSB_CHANGE_BITS ((1 << 17) | (1 << 18) | (1 << 20) | (1 << 21) | (1 << 22))

// ======= Port status and control registers END ====

// 端口信息标志位
#define XHCI_PROTOCOL_USB2 0
#define XHCI_PROTOCOL_USB3 1
#define XHCI_PROTOCOL_INFO (1 << 0)     // 1->usb3, 0->usb2
#define XHCI_PROTOCOL_HSO (1 << 1)      // 1-> usb2 high speed only
#define XHCI_PROTOCOL_HAS_PAIR (1 << 2) // 当前位被置位，意味着当前端口具有一个与之配对的端口
#define XHCI_PROTOCOL_ACTIVE (1 << 3)   // 当前端口是这个配对中，被激活的端口

/**
 * @brief xhci端口信息
 *
 */
struct xhci_port_info_t
{
    uint8_t flags;           // port flags
    uint8_t paired_port_num; // 与当前端口所配对的另一个端口（相同物理接口的不同速度的port）
    uint8_t offset;          // offset of this port within this protocal
    uint8_t reserved;
} __attribute__((packed));

struct xhci_host_controller_t
{
    struct pci_device_structure_general_device_t *pci_dev_hdr; // 指向pci header结构体的指针
    int controller_id;                                         // 操作系统给controller的编号
    uint64_t vbase;                                            // 虚拟地址base（bar0映射到的虚拟地址）
    uint64_t vbase_op;                                         // Operational registers 起始虚拟地址
    uint32_t rts_offset;                                       // Runtime Register Space offset
    uint32_t db_offset;                                        // Doorbell offset
    uint32_t ext_caps_off;                                     // 扩展能力寄存器偏移量
    uint8_t context_size;                                      // 上下文大小
    uint16_t port_num;                                         // 总的端口数量
    uint8_t port_num_u2;                                       // usb 2.0端口数量
    uint8_t port_num_u3;                                       // usb 3端口数量
    uint32_t page_size;                                        // page size
    uint64_t dcbaap_vaddr;                                     // Device Context Base Address Array Pointer的虚拟地址
    uint64_t cmd_ring_vaddr;                                   // command ring的虚拟地址
    uint64_t event_ring_vaddr;                                 // event ring的虚拟地址
    uint64_t event_ring_table_vaddr;                           // event ring table的虚拟地址
    uint8_t cmd_trb_cycle;                                     // 当前command ring cycle
    uint8_t current_event_ring_cycle;                          // 当前event ring cycle
    struct xhci_port_info_t ports[XHCI_MAX_ROOT_HUB_PORTS];    // 指向端口信息数组的指针(由于端口offset是从1开始的，因此该数组第0项为空)
};

/**
 * @brief 初始化xhci控制器
 *
 * @param header 指定控制器的pci device头部
 */
void xhci_init(struct pci_device_structure_general_device_t *header);