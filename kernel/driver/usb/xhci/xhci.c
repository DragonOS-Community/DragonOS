#include "xhci.h"
#include <common/kprint.h>
#include <debug/bug.h>
#include <common/spinlock.h>
#include <mm/mm.h>
#include <mm/slab.h>
#include <debug/traceback/traceback.h>
#include <common/time.h>
#include <exception/irq.h>
#include <driver/interrupt/apic/apic.h>

// 由于xhci寄存器读取需要对齐，因此禁用GCC优化选项
#pragma GCC optimize("O0")

spinlock_t xhci_controller_init_lock = {0}; // xhci控制器初始化锁(在usb_init中被初始化)

static int xhci_ctrl_count = 0; // xhci控制器计数

static struct xhci_host_controller_t xhci_hc[XHCI_MAX_HOST_CONTROLLERS] = {0};

void xhci_hc_irq_enable(uint64_t irq_num);
void xhci_hc_irq_disable(uint64_t irq_num);
uint64_t xhci_hc_irq_install(uint64_t irq_num, void *arg);
void xhci_hc_irq_uninstall(uint64_t irq_num);

static int xhci_hc_find_available_id();
static int xhci_hc_stop(int id);
static int xhci_hc_reset(int id);
static int xhci_hc_stop_legacy(int id);
static int xhci_hc_start_sched(int id);
static int xhci_hc_stop_sched(int id);
static uint32_t xhci_hc_get_protocol_offset(int id, uint32_t list_off, const int version, uint32_t *offset, uint32_t *count, uint16_t *protocol_flag);
static int xhci_hc_pair_ports(int id);
static uint64_t xhci_create_ring(int trbs);
static uint64_t xhci_create_event_ring(int trbs, uint64_t *ret_ring_addr);
void xhci_hc_irq_handler(uint64_t irq_num, uint64_t cid, struct pt_regs *regs);
static int xhci_hc_init_intr(int id);
static int xhci_hc_start_ports(int id);

static int xhci_send_command(int id, struct xhci_TRB_t *trb, const bool do_ring);
static uint64_t xhci_initialize_slot(const int id, const int slot_id, const int port, const int speed, const int max_packet);
static void xhci_initialize_ep(const int id, const uint64_t slot_vaddr, const int slot_id, const int ep_num, const int max_packet, const int type, const int direction, const int speed, const int ep_interval);
static int xhci_set_address(const int id, const uint64_t slot_vaddr, const int slot_id, const bool block);
static int xhci_control_in(const int id, void *target, const int in_size, const int slot_id, const int max_packet);
static int xhci_setup_stage(struct xhci_ep_ring_info_t *ep, const struct usb_request_packet_t *packet, const uint8_t direction);
static int xhci_data_stage(struct xhci_ep_ring_info_t *ep, uint64_t buf_vaddr, uint8_t trb_type, const uint32_t size, uint8_t direction, const int max_packet, const uint64_t status_vaddr);
static int xhci_status_stage(const int id, uint8_t direction, uint64_t status_buf_vaddr);
static int xhci_wait_for_interrupt(const int id, uint64_t status_vaddr);

static struct xhci_ep_context_t ep_ctx = {0};
struct xhci_slot_context_t slot_ctx = {0};

hardware_intr_controller xhci_hc_intr_controller =
    {
        .enable = xhci_hc_irq_enable,
        .disable = xhci_hc_irq_disable,
        .install = xhci_hc_irq_install,
        .uninstall = xhci_hc_irq_uninstall,
        .ack = apic_local_apic_edge_ack,
};

/*
    注意！！！

    尽管采用MMI/O的方式访问寄存器，但是对于指定大小的寄存器，
    在发起读请求的时候，只能从寄存器的起始地址位置开始读取。

    例子：不能在一个32bit的寄存器中的偏移量8的位置开始读取1个字节
            这种情况下，我们必须从32bit的寄存器的0地址处开始读取32bit，然后通过移位的方式得到其中的字节。
*/

#define xhci_read_cap_reg32(id, offset) (__read4b(xhci_hc[id].vbase + (offset)))
#define xhci_get_ptr_cap_reg32(id, offset) ((uint32_t *)(xhci_hc[id].vbase + (offset)))
#define xhci_write_cap_reg32(id, offset, value) (__write4b(xhci_hc[id].vbase + (offset), (value)))

#define xhci_read_cap_reg64(id, offset) (__read8b(xhci_hc[id].vbase + (offset)))
#define xhci_get_ptr_reg64(id, offset) ((uint64_t *)(xhci_hc[id].vbase + (offset)))
#define xhci_write_cap_reg64(id, offset, value) (__write8b(xhci_hc[id].vbase + (offset), (value)))

#define xhci_read_op_reg8(id, offset) (*(uint8_t *)(xhci_hc[id].vbase_op + (offset)))
#define xhci_get_ptr_op_reg8(id, offset) ((uint8_t *)(xhci_hc[id].vbase_op + (offset)))
#define xhci_write_op_reg8(id, offset, value) (*(uint8_t *)(xhci_hc[id].vbase_op + (offset)) = (uint8_t)(value))

#define xhci_read_op_reg32(id, offset) (__read4b(xhci_hc[id].vbase_op + (offset)))
#define xhci_get_ptr_op_reg32(id, offset) ((uint32_t *)(xhci_hc[id].vbase_op + (offset)))
#define xhci_write_op_reg32(id, offset, value) (__write4b(xhci_hc[id].vbase_op + (offset), (value)))

#define xhci_read_op_reg64(id, offset) (__read8b(xhci_hc[id].vbase_op + (offset)))
#define xhci_get_ptr_op_reg64(id, offset) ((uint64_t *)(xhci_hc[id].vbase_op + (offset)))
#define xhci_write_op_reg64(id, offset, value) (__write8b(xhci_hc[id].vbase_op + (offset), (value)))

/**
 * @brief 计算中断寄存器组虚拟地址
 * @param id 主机控制器id
 * @param num xhci中断寄存器组号
 */
#define xhci_calc_intr_vaddr(id, num) (xhci_hc[id].vbase + xhci_hc[id].rts_offset + XHCI_RT_IR0 + (num)*XHCI_IR_SIZE)
/**
 * @brief 读取/写入中断寄存器
 * @param id 主机控制器id
 * @param num xhci中断寄存器组号
 * @param intr_offset 寄存器在当前寄存器组中的偏移量
 */
#define xhci_read_intr_reg32(id, num, intr_offset) (__read4b(xhci_calc_intr_vaddr(id, num) + (intr_offset)))
#define xhci_write_intr_reg32(id, num, intr_offset, value) (__write4b(xhci_calc_intr_vaddr(id, num) + (intr_offset), (value)))
#define xhci_read_intr_reg64(id, num, intr_offset) (__read8b(xhci_calc_intr_vaddr(id, num) + (intr_offset)))
#define xhci_write_intr_reg64(id, num, intr_offset, value) (__write8b(xhci_calc_intr_vaddr(id, num) + (intr_offset), (value)))

#define xhci_is_aligned64(addr) (((addr)&0x3f) == 0) // 是否64bytes对齐

/**
 * @brief 判断端口信息
 * @param cid 主机控制器id
 * @param pid 端口id
 */
#define XHCI_PORT_IS_USB2(cid, pid) ((xhci_hc[cid].ports[pid].flags & XHCI_PROTOCOL_INFO) == XHCI_PROTOCOL_USB2)
#define XHCI_PORT_IS_USB3(cid, pid) ((xhci_hc[cid].ports[pid].flags & XHCI_PROTOCOL_INFO) == XHCI_PROTOCOL_USB3)

#define XHCI_PORT_IS_USB2_HSO(cid, pid) ((xhci_hc[cid].ports[pid].flags & XHCI_PROTOCOL_HSO) == XHCI_PROTOCOL_HSO)
#define XHCI_PORT_HAS_PAIR(cid, pid) ((xhci_hc[cid].ports[pid].flags & XHCI_PROTOCOL_HAS_PAIR) == XHCI_PROTOCOL_HAS_PAIR)
#define XHCI_PORT_IS_ACTIVE(cid, pid) ((xhci_hc[cid].ports[pid].flags & XHCI_PROTOCOL_ACTIVE) == XHCI_PROTOCOL_ACTIVE)

/**
 * @brief 设置link TRB的命令（dword3）
 *
 */
#define xhci_TRB_set_link_cmd(trb_vaddr)                                         \
    do                                                                           \
    {                                                                            \
        struct xhci_TRB_normal_t *ptr = (struct xhci_TRB_normal_t *)(trb_vaddr); \
        ptr->TRB_type = TRB_TYPE_LINK;                                           \
        ptr->ioc = 0;                                                            \
        ptr->chain = 0;                                                          \
        ptr->ent = 0;                                                            \
        ptr->cycle = 1;                                                          \
    } while (0)

// 设置endpoint结构体的dequeue_cycle_state bit
#define xhci_ep_set_dequeue_cycle_state(ep_ctx_ptr, state) ((ep_ctx_ptr)->tr_dequeue_ptr |= ((state)&1))
// 获取endpoint结构体的dequeue_cycle_state bit
#define xhci_ep_get_dequeue_cycle_state(ep_ctx_ptr) (((ep_ctx_ptr)->tr_dequeue_ptr) & 1)

/**
 * @brief 在controller数组之中寻找可用插槽
 *
 * 注意：该函数只能被获得init锁的进程所调用
 * @return int 可用id(无空位时返回-1)
 */
static int xhci_hc_find_available_id()
{
    if (unlikely(xhci_ctrl_count >= XHCI_MAX_HOST_CONTROLLERS))
        return -1;

    for (int i = 0; i < XHCI_MAX_HOST_CONTROLLERS; ++i)
    {
        if (xhci_hc[i].pci_dev_hdr == NULL)
            return i;
    }
    return -1;
}

/**
 * @brief 从指定地址读取trb
 *
 * @param trb 要存储到的trb的地址
 * @param address 待读取trb的地址
 */
static __always_inline void xhci_get_trb(struct xhci_TRB_t *trb, const uint64_t address)
{
    trb->param = __read8b(address);
    trb->status = __read4b(address + 8);
    trb->command = __read4b(address + 12);
}

/**
 * @brief 将给定的trb写入指定的地址
 *
 * @param trb 源trb
 * @param address 拷贝的目标地址
 */
static __always_inline void xhci_set_trb(struct xhci_TRB_t *trb, const uint64_t address)
{
    __write8b(address, trb->param);
    __write4b(address + 8, trb->status);
    __write4b(address + 12, trb->command);
}

/**
 * @brief 将ep结构体写入到设备上下文中的对应块内
 *
 * @param id 主机控制器id
 * @param slot_vaddr 设备上下文虚拟地址
 * @param ep_num ep结构体要写入到哪个块中（在设备上下文中的块号）
 * @param ep 源数据
 */
static __always_inline void __write_ep(int id, uint64_t slot_vaddr, int ep_num, struct xhci_ep_context_t *ep)
{
    memcpy((void *)(slot_vaddr + ep_num * xhci_hc[id].context_size), ep, sizeof(struct xhci_ep_context_t));
}

/**
 * @brief 从设备上下文中的对应块内读取数据到ep结构体
 *
 * @param id 主机控制器id
 * @param slot_vaddr 设备上下文虚拟地址
 * @param ep_num 要从哪个块中读取（在设备上下文中的块号）
 * @param ep 目标地址
 */
static __always_inline void __read_from_ep(int id, uint64_t slot_vaddr, int ep_num, struct xhci_ep_context_t *ep)
{
    memcpy(ep, (void *)(slot_vaddr + ep_num * xhci_hc[id].context_size), sizeof(struct xhci_ep_context_t));
}

/**
 * @brief 将slot上下文数组结构体写入插槽的上下文空间
 *
 * @param vaddr 目标地址
 * @param slot_ctx slot上下文数组
 */
static __always_inline void __write_slot(const uint64_t vaddr, struct xhci_slot_context_t *slot_ctx)
{
    memcpy((void *)vaddr, slot_ctx, sizeof(struct xhci_slot_context_t));
}

/**
 * @brief 从指定地址读取slot context
 *
 * @param slot_ctx 目标地址
 * @param slot_vaddr 源地址
 * @return __always_inline
 */
static __always_inline void __read_from_slot(struct xhci_slot_context_t *slot_ctx, uint64_t slot_vaddr)
{
    memcpy(slot_ctx, (void *)slot_vaddr, sizeof(struct xhci_slot_context_t));
}

/**
 * @brief 写入doorbell寄存器
 *
 * @param id 主机控制器id
 * @param slot_id usb控制器插槽id（0用作命令门铃，其他的用于具体的设备的门铃）
 * @param value endpoint
 */
static __always_inline void __xhci_write_doorbell(const int id, const uint16_t slot_id, const uint32_t value)
{
    // 确保写入门铃寄存器之前，所有的写操作均已完成
    io_mfence();
    xhci_write_cap_reg32(id, xhci_hc[id].db_offset + slot_id * sizeof(uint32_t), value);
    io_mfence();
}

/**
 * @brief 将trb写入指定的ring中，并更新下一个要写入的地址的值
 *
 * @param ep_info 端点信息结构体
 * @param trb 待写入的trb
 */
static __always_inline void __xhci_write_trb(struct xhci_ep_ring_info_t *ep_info, struct xhci_TRB_t *trb)
{
    memcpy((void *)ep_info->current_ep_ring_vaddr, trb, sizeof(struct xhci_TRB_t));

    ep_info->current_ep_ring_vaddr += sizeof(struct xhci_TRB_t);

    struct xhci_TRB_normal_t *ptr = (struct xhci_TRB_normal_t *)(ep_info->current_ep_ring_vaddr);

    // ring到头了，转换cycle，然后回到第一个trb
    if (unlikely(ptr->TRB_type == TRB_TYPE_LINK))
    {
        ptr->cycle = ep_info->current_ep_ring_cycle;
        ep_info->current_ep_ring_vaddr = ep_info->ep_ring_vbase;
        ep_info->current_ep_ring_cycle ^= 1;
    }
}

/**
 * @brief 停止xhci主机控制器
 *
 * @param id 主机控制器id
 * @return int
 */
static int xhci_hc_stop(int id)
{

    // 判断是否已经停止
    if (unlikely((xhci_read_op_reg32(id, XHCI_OPS_USBSTS) & (1 << 0)) == 1))
        return 0;
    io_mfence();
    xhci_write_op_reg32(id, XHCI_OPS_USBCMD, 0x00000000);
    io_mfence();
    char timeout = 17;
    while ((xhci_read_op_reg32(id, XHCI_OPS_USBSTS) & (1 << 0)) == 0)
    {
        io_mfence();
        usleep(1000);
        if (--timeout == 0)
            return -ETIMEDOUT;
    }

    return 0;
}

/**
 * @brief reset xHCI主机控制器
 *
 * @param id 主机控制器id
 * @return int
 */
static int xhci_hc_reset(int id)
{
    int retval = 0;
    io_mfence();
    // 判断HCHalted是否置位
    if ((xhci_read_op_reg32(id, XHCI_OPS_USBSTS) & (1 << 0)) == 0)
    {
        io_mfence();
        kdebug("stopping usb hc...");
        // 未置位，需要先尝试停止usb主机控制器
        retval = xhci_hc_stop(id);
        if (unlikely(retval))
            return retval;
    }
    int timeout = 500; // wait 500ms
    // reset
    uint32_t cmd = xhci_read_op_reg32(id, XHCI_OPS_USBCMD);
    io_mfence();

    cmd |= (1 << 1);
    xhci_write_op_reg32(id, XHCI_OPS_USBCMD, cmd);
    io_mfence();
    io_mfence();
    while (xhci_read_op_reg32(id, XHCI_OPS_USBCMD) & (1 << 1))
    {
        io_mfence();
        usleep(1000);
        if (--timeout == 0)
            return -ETIMEDOUT;
    }

    return retval;
}

/**
 * @brief 停止指定xhci控制器的legacy support
 *
 * @param id 控制器id
 * @return int
 */
static int xhci_hc_stop_legacy(int id)
{
    uint64_t current_offset = xhci_hc[id].ext_caps_off;

    do
    {
        // 判断当前entry是否为legacy support entry
        if ((xhci_read_cap_reg32(id, current_offset) & 0xff) == XHCI_XECP_ID_LEGACY)
        {
            io_mfence();
            // 接管控制权
            xhci_write_cap_reg32(id, current_offset, xhci_read_cap_reg32(id, current_offset) | XHCI_XECP_LEGACY_OS_OWNED);
            io_mfence();
            // 等待响应完成
            int timeout = XHCI_XECP_LEGACY_TIMEOUT;
            while ((xhci_read_cap_reg32(id, current_offset) & XHCI_XECP_LEGACY_OWNING_MASK) != XHCI_XECP_LEGACY_OS_OWNED)
            {
                io_mfence();
                usleep(1000);
                if (--timeout == 0)
                {
                    kerror("The BIOS doesn't stop legacy support.");
                    return -ETIMEDOUT;
                }
            }
            // 处理完成
            return 0;
        }
        io_mfence();
        // 读取下一个entry的偏移增加量
        int next_off = ((xhci_read_cap_reg32(id, current_offset) & 0xff00) >> 8) << 2;
        io_mfence();
        // 将指针跳转到下一个entry
        current_offset = next_off ? (current_offset + next_off) : 0;
    } while (current_offset);

    // 当前controller不存在legacy支持，也问题不大，不影响
    return 0;
}

/**
 * @brief 启用指定xhci控制器的调度
 *
 * @param id 控制器id
 * @return int
 */
static int xhci_hc_start_sched(int id)
{
    io_mfence();
    xhci_write_op_reg32(id, XHCI_OPS_USBCMD, (1 << 0) | (1 << 2) | (1 << 3));
    io_mfence();
    usleep(100 * 1000);
}

/**
 * @brief 停止指定xhci控制器的调度
 *
 * @param id 控制器id
 * @return int
 */
static int xhci_hc_stop_sched(int id)
{
    io_mfence();
    xhci_write_op_reg32(id, XHCI_OPS_USBCMD, 0x00);
    io_mfence();
}

/**
 * @brief 在Ex capability list中寻找符合指定的协议号的寄存器offset、count、flag信息
 *
 * @param id 主机控制器id
 * @param list_off 列表项位置距离控制器虚拟基地址的偏移量
 * @param version 要寻找的端口版本号（2或3）
 * @param offset 返回的 Compatible Port Offset
 * @param count 返回的 Compatible Port Count
 * @param protocol_flag 返回的与协议相关的flag
 * @return uint32_t 下一个列表项的偏移量
 */
static uint32_t xhci_hc_get_protocol_offset(int id, uint32_t list_off, const int version, uint32_t *offset, uint32_t *count, uint16_t *protocol_flag)
{
    if (count)
        *count = 0;

    do
    {
        uint32_t dw0 = xhci_read_cap_reg32(id, list_off);
        io_mfence();
        uint32_t next_list_off = (dw0 >> 8) & 0xff;
        next_list_off = next_list_off ? (list_off + (next_list_off << 2)) : 0;

        if ((dw0 & 0xff) == XHCI_XECP_ID_PROTOCOL && ((dw0 & 0xff000000) >> 24) == version)
        {
            uint32_t dw2 = xhci_read_cap_reg32(id, list_off + 8);
            io_mfence();
            if (offset != NULL)
                *offset = (uint32_t)(dw2 & 0xff) - 1; // 使其转换为zero based
            if (count != NULL)
                *count = (uint32_t)((dw2 & 0xff00) >> 8);
            if (protocol_flag != NULL && version == 2)
                *protocol_flag = (uint16_t)((dw2 >> 16) & 0x0fff);

            return next_list_off;
        }

        list_off = next_list_off;
    } while (list_off);

    return 0;
}
/**
 * @brief 配对xhci主机控制器的usb2、usb3端口
 *
 * @param id 主机控制器id
 * @return int 返回码
 */
static int xhci_hc_pair_ports(int id)
{

    struct xhci_caps_HCSPARAMS1_reg_t hcs1;
    io_mfence();
    memcpy(&hcs1, xhci_get_ptr_cap_reg32(id, XHCI_CAPS_HCSPARAMS1), sizeof(struct xhci_caps_HCSPARAMS1_reg_t));
    io_mfence();
    // 从hcs1获取端口数量
    xhci_hc[id].port_num = hcs1.max_ports;

    // 找到所有的端口并标记其端口信息

    xhci_hc[id].port_num_u2 = 0;
    xhci_hc[id].port_num_u3 = 0;

    uint32_t next_off = xhci_hc[id].ext_caps_off;
    uint32_t offset, cnt;
    uint16_t protocol_flags = 0;

    // 寻找所有的usb2端口
    while (next_off)
    {
        io_mfence();
        next_off = xhci_hc_get_protocol_offset(id, next_off, 2, &offset, &cnt, &protocol_flags);
        io_mfence();

        if (cnt)
        {
            for (int i = 0; i < cnt; ++i)
            {
                io_mfence();
                xhci_hc[id].ports[offset + i].offset = xhci_hc[id].port_num_u2++;
                xhci_hc[id].ports[offset + i].flags = XHCI_PROTOCOL_USB2;
                io_mfence();
                // usb2 high speed only
                if (protocol_flags & 2)
                    xhci_hc[id].ports[offset + i].flags |= XHCI_PROTOCOL_HSO;
            }
        }
    }

    // 寻找所有的usb3端口
    next_off = xhci_hc[id].ext_caps_off;
    while (next_off)
    {
        io_mfence();
        next_off = xhci_hc_get_protocol_offset(id, next_off, 3, &offset, &cnt, &protocol_flags);
        io_mfence();

        if (cnt)
        {
            for (int i = 0; i < cnt; ++i)
            {
                io_mfence();
                xhci_hc[id].ports[offset + i].offset = xhci_hc[id].port_num_u3++;
                xhci_hc[id].ports[offset + i].flags = XHCI_PROTOCOL_USB3;
            }
        }
    }

    // 将对应的USB2端口和USB3端口进行配对
    for (int i = 0; i < xhci_hc[id].port_num; ++i)
    {
        for (int j = 0; j < xhci_hc[id].port_num; ++j)
        {
            if (unlikely(i == j))
                continue;
            io_mfence();
            if ((xhci_hc[id].ports[i].offset == xhci_hc[id].ports[j].offset) &&
                ((xhci_hc[id].ports[i].flags & XHCI_PROTOCOL_INFO) != (xhci_hc[id].ports[j].flags & XHCI_PROTOCOL_INFO)))
            {
                xhci_hc[id].ports[i].paired_port_num = j;
                xhci_hc[id].ports[i].flags |= XHCI_PROTOCOL_HAS_PAIR;
                io_mfence();
                xhci_hc[id].ports[j].paired_port_num = i;
                xhci_hc[id].ports[j].flags |= XHCI_PROTOCOL_HAS_PAIR;
            }
        }
    }

    // 标记所有的usb3、单独的usb2端口为激活状态
    for (int i = 0; i < xhci_hc[id].port_num; ++i)
    {
        io_mfence();
        if (XHCI_PORT_IS_USB3(id, i) ||
            (XHCI_PORT_IS_USB2(id, i) && (!XHCI_PORT_HAS_PAIR(id, i))))
            xhci_hc[id].ports[i].flags |= XHCI_PROTOCOL_ACTIVE;
    }
    kinfo("Found %d ports on root hub, usb2 ports:%d, usb3 ports:%d", xhci_hc[id].port_num, xhci_hc[id].port_num_u2, xhci_hc[id].port_num_u3);

    /*
    // 打印配对结果
    for (int i = 1; i <= xhci_hc[id].port_num; ++i)
    {
        if (XHCI_PORT_IS_USB3(id, i))
        {
            kdebug("USB3 port %d, offset=%d, pair with usb2 port %d, current port is %s", i, xhci_hc[id].ports[i].offset,
                   xhci_hc[id].ports[i].paired_port_num, XHCI_PORT_IS_ACTIVE(id, i) ? "active" : "inactive");
        }
        else if (XHCI_PORT_IS_USB2(id, i) && (!XHCI_PORT_HAS_PAIR(id, i))) // 单独的2.0接口
        {
            kdebug("Stand alone USB2 port %d, offset=%d, current port is %s", i, xhci_hc[id].ports[i].offset,
                   XHCI_PORT_IS_ACTIVE(id, i) ? "active" : "inactive");
        }
        else if (XHCI_PORT_IS_USB2(id, i))
        {
            kdebug("USB2 port %d, offset=%d, current port is %s, has pair=%s", i, xhci_hc[id].ports[i].offset,
                   XHCI_PORT_IS_ACTIVE(id, i) ? "active" : "inactive", XHCI_PORT_HAS_PAIR(id, i) ? "true" : "false");
        }
    }
    */

    return 0;
}

/**
 * @brief 创建ring，并将最后一个trb指向头一个trb
 *
 * @param trbs 要创建的trb数量
 * @return uint64_t trb数组的起始虚拟地址
 */
static uint64_t xhci_create_ring(int trbs)
{
    int total_size = trbs * sizeof(struct xhci_TRB_t);
    const uint64_t vaddr = (uint64_t)kmalloc(total_size, 0);
    io_mfence();
    memset((void *)vaddr, 0, total_size);
    io_mfence();
    // 设置最后一个trb为link trb
    xhci_TRB_set_link_cmd(vaddr + total_size - sizeof(struct xhci_TRB_t));
    io_mfence();
    return vaddr;
}

/**
 * @brief 创建新的event ring table和对应的ring segment
 *
 * @param trbs 包含的trb的数量
 * @param ret_ring_addr 返回的第一个event ring segment的基地址（虚拟）
 * @return uint64_t trb table的虚拟地址
 */
static uint64_t xhci_create_event_ring(int trbs, uint64_t *ret_ring_addr)
{
    const uint64_t table_vaddr = (const uint64_t)kmalloc(64, 0); // table支持8个segment
    io_mfence();
    if (unlikely(table_vaddr == NULL))
        return -ENOMEM;
    memset((void *)table_vaddr, 0, 64);

    // 暂时只创建1个segment
    const uint64_t seg_vaddr = (const uint64_t)kmalloc(trbs * sizeof(struct xhci_TRB_t), 0);
    io_mfence();
    if (unlikely(seg_vaddr == NULL))
        return -ENOMEM;

    memset((void *)seg_vaddr, 0, trbs * sizeof(struct xhci_TRB_t));
    io_mfence();
    // 将segment地址和大小写入table
    *(uint64_t *)(table_vaddr) = virt_2_phys(seg_vaddr);
    *(uint64_t *)(table_vaddr + 8) = trbs;

    *ret_ring_addr = seg_vaddr;
    return table_vaddr;
}

void xhci_hc_irq_enable(uint64_t irq_num)
{
    int cid = xhci_find_hcid_by_irq_num(irq_num);
    io_mfence();
    if (WARN_ON(cid == -1))
        return;

    io_mfence();
    pci_start_msi(xhci_hc[cid].pci_dev_hdr);

    io_mfence();
    xhci_hc_start_sched(cid);
    io_mfence();
    xhci_hc_start_ports(cid);
}

void xhci_hc_irq_disable(uint64_t irq_num)
{
    int cid = xhci_find_hcid_by_irq_num(irq_num);
    io_mfence();
    if (WARN_ON(cid == -1))
        return;

    xhci_hc_stop_sched(cid);
    io_mfence();
    pci_disable_msi(xhci_hc[cid].pci_dev_hdr);
    io_mfence();
}

/**
 * @brief xhci中断的安装函数
 *
 * @param irq_num 要安装的中断向量号
 * @param arg 参数
 * @return uint64_t 错误码
 */
uint64_t xhci_hc_irq_install(uint64_t irq_num, void *arg)
{
    int cid = xhci_find_hcid_by_irq_num(irq_num);
    io_mfence();
    if (WARN_ON(cid == -1))
        return -EINVAL;

    struct xhci_hc_irq_install_info_t *info = (struct xhci_hc_irq_install_info_t *)arg;
    struct msi_desc_t msi_desc;
    memset(&msi_desc, 0, sizeof(struct msi_desc_t));
    io_mfence();
    msi_desc.irq_num = irq_num;
    msi_desc.msi_index = 0;
    msi_desc.pci_dev = (struct pci_device_structure_header_t *)xhci_hc[cid].pci_dev_hdr;
    msi_desc.assert = info->assert;
    msi_desc.edge_trigger = info->edge_trigger;
    msi_desc.processor = info->processor;
    msi_desc.pci.msi_attribute.is_64 = 1;
    msi_desc.pci.msi_attribute.is_msix = 1;
    io_mfence();
    int retval = pci_enable_msi(&msi_desc);

    return 0;
}

void xhci_hc_irq_uninstall(uint64_t irq_num)
{
    // todo
    int cid = xhci_find_hcid_by_irq_num(irq_num);
    io_mfence();
    if (WARN_ON(cid == -1))
        return;
    xhci_hc_stop(cid);
    io_mfence();
}
/**
 * @brief xhci主机控制器的中断处理函数
 *
 * @param irq_num 中断向量号
 * @param cid 控制器号
 * @param regs 寄存器值
 */
void xhci_hc_irq_handler(uint64_t irq_num, uint64_t cid, struct pt_regs *regs)
{
    // kdebug("USB irq received.");
    /*
        写入usb status寄存器，以表明当前收到了中断,清除usb status寄存器中的EINT位
        需要先清除这个位，再清除interrupter中的pending bit）
    */
    xhci_write_op_reg32(cid, XHCI_OPS_USBSTS, xhci_read_op_reg32(cid, XHCI_OPS_USBSTS));

    // 读取第0个usb interrupter的intr management寄存器
    const uint32_t iman0 = xhci_read_intr_reg32(cid, 0, XHCI_IR_MAN);
    uint64_t dequeue_reg = xhci_read_intr_reg64(cid, 0, XHCI_IR_DEQUEUE);

    if (((iman0 & 3) == 3) || (dequeue_reg & 8)) // 中断被启用，且pending不为0
    {
        // 写入1以清除该interrupter的pending bit
        xhci_write_intr_reg32(cid, 0, XHCI_IR_MAN, iman0 | 3);
        io_mfence();
        struct xhci_TRB_t event_trb, origin_trb; // event ring trb以及其对应的command trb
        uint64_t origin_vaddr;
        // 暂存当前trb的起始地址
        uint64_t last_event_ring_vaddr = xhci_hc[cid].current_event_ring_vaddr;
        xhci_get_trb(&event_trb, xhci_hc[cid].current_event_ring_vaddr);
        while ((event_trb.command & 1) == xhci_hc[cid].current_event_ring_cycle) // 循环处理处于当前周期的所有event ring
        {

            struct xhci_TRB_cmd_complete_t *event_trb_ptr = (struct xhci_TRB_cmd_complete_t *)&event_trb;
            if ((event_trb.command & (1 << 2)) == 0) // 当前event trb不是由于short packet产生的
            {

                switch (event_trb_ptr->code) // 判断它的完成码
                {
                case TRB_COMP_TRB_SUCCESS: // trb执行成功，则将结果返回到对应的command ring的trb里面

                    switch (event_trb_ptr->TRB_type) // 根据event trb类型的不同，采取不同的措施
                    {
                    case TRB_TYPE_COMMAND_COMPLETION: // 命令已经完成
                        origin_vaddr = (uint64_t)phys_2_virt(event_trb.param);
                        // 获取对应的command trb
                        xhci_get_trb(&origin_trb, origin_vaddr);

                        switch (((struct xhci_TRB_normal_t *)&origin_trb)->TRB_type)
                        {
                        case TRB_TYPE_ENABLE_SLOT: // 源命令为enable slot
                            // 将slot id返回到命令TRB的command字段中
                            origin_trb.command &= 0x00ffffff;
                            origin_trb.command |= (event_trb.command & 0xff000000);
                            origin_trb.status = event_trb.status;
                            break;
                        default:
                            origin_trb.status = event_trb.status;
                            break;
                        }

                        // 标记该命令已经执行完成
                        origin_trb.status |= XHCI_IRQ_DONE;
                        // 将command trb写入到表中
                        xhci_set_trb(&origin_trb, origin_vaddr);
                        // kdebug("set origin:%#018lx", origin_vaddr);
                        break;
                    }
                    break;

                default:
                    break;
                }
            }
            else // 当前TRB是由short packet产生的
            {
                switch (event_trb_ptr->TRB_type)
                {
                case TRB_TYPE_TRANS_EVENT: // 当前 event trb是 transfer event TRB
                    // If SPD was encountered in this TD, comp_code will be SPD, else it should be SUCCESS (specs 4.10.1.1)
                    __write4b((uint64_t)phys_2_virt(event_trb.param), (event_trb.status | XHCI_IRQ_DONE)); // return code + bytes *not* transferred
                    break;

                default:
                    break;
                }
            }

            // 获取下一个event ring TRB
            last_event_ring_vaddr = xhci_hc[cid].current_event_ring_vaddr;
            xhci_hc[cid].current_event_ring_vaddr += sizeof(struct xhci_TRB_t);
            xhci_get_trb(&event_trb, xhci_hc[cid].current_event_ring_vaddr);
            if (((struct xhci_TRB_normal_t *)&event_trb)->TRB_type == TRB_TYPE_LINK)
            {
                xhci_hc[cid].current_event_ring_vaddr = xhci_hc[cid].event_ring_vaddr;
                xhci_get_trb(&event_trb, xhci_hc[cid].current_event_ring_vaddr);
            }
        }

        // 当前event ring cycle的TRB处理结束
        // 更新dequeue指针, 并清除event handler busy标志位
        xhci_write_intr_reg64(cid, 0, XHCI_IR_DEQUEUE, virt_2_phys(last_event_ring_vaddr) | (1 << 3));
        io_mfence();
    }
}
/**
 * @brief 重置端口
 *
 * @param id 控制器id
 * @param port 端口id
 * @return int
 */
static int xhci_reset_port(const int id, const int port)
{
    int retval = 0;
    // 相对于op寄存器基地址的偏移量
    uint64_t port_status_offset = XHCI_OPS_PRS + port * 16;
    // kdebug("to reset %d, portsc=%#010lx", port, (xhci_read_op_reg32(id, port_status_offset + XHCI_PORT_PORTSC)));
    // kdebug("to reset %d, usbcmd=%#010lx", port, xhci_read_op_reg32(id, XHCI_OPS_USBCMD));
    // kdebug("to reset %d, usbsts=%#010lx", port, xhci_read_op_reg32(id, XHCI_OPS_USBSTS));
    io_mfence();
    // 检查端口电源状态
    if ((xhci_read_op_reg32(id, port_status_offset + XHCI_PORT_PORTSC) & (1 << 9)) == 0)
    {
        kdebug("port is power off, starting...");
        io_mfence();
        xhci_write_cap_reg32(id, port_status_offset + XHCI_PORT_PORTSC, (1 << 9));
        io_mfence();
        usleep(2000);
        // 检测端口是否被启用, 若未启用，则报错
        if ((xhci_read_op_reg32(id, port_status_offset + XHCI_PORT_PORTSC) & (1 << 9)) == 0)
        {
            kdebug("cannot power on %d", port);
            return -EAGAIN;
        }
    }
    // kdebug("port:%d, power check ok", port);
    io_mfence();
    // 确保端口的status被清0
    xhci_write_op_reg32(id, port_status_offset + XHCI_PORT_PORTSC, (1 << 9) | XHCI_PORTUSB_CHANGE_BITS);
    // kdebug("to reset timeout;");
    io_mfence();
    // 重置当前端口
    if (XHCI_PORT_IS_USB3(id, port))
        xhci_write_op_reg32(id, port_status_offset + XHCI_PORT_PORTSC, (1 << 9) | (1 << 31));
    else
        xhci_write_op_reg32(id, port_status_offset + XHCI_PORT_PORTSC, (1 << 9) | (1 << 4));

    retval = -ETIMEDOUT;
    // kdebug("to wait reset timeout;");
    // 等待portsc的port reset change位被置位，说明reset完成
    int timeout = 200;
    while (timeout)
    {
        io_mfence();
        uint32_t val = xhci_read_op_reg32(id, port_status_offset + XHCI_PORT_PORTSC);
        // kdebug("val=%#010lx", val);
        io_mfence();
        if (XHCI_PORT_IS_USB3(id, port) && (val & (1 << 31)) == 0)
            break;
        else if (XHCI_PORT_IS_USB2(id, port) && (val & (1 << 4)) == 0)
            break;
        else if (val & (1 << 21))
            break;

        --timeout;
        usleep(500);
    }
    // kdebug("timeout= %d", timeout);

    if (timeout > 0)
    {
        // 等待恢复
        usleep(USB_TIME_RST_REC * 100);
        // kdebug("to check if reset ok");
        uint32_t val = xhci_read_op_reg32(id, port_status_offset + XHCI_PORT_PORTSC);
        io_mfence();

        // 如果reset之后，enable bit仍然是1，那么说明reset成功
        if (val & (1 << 1))
        {
            // kdebug("reset ok");
            retval = 0;
            io_mfence();
            // 清除status change bit
            xhci_write_op_reg32(id, port_status_offset + XHCI_PORT_PORTSC, (1 << 9) | XHCI_PORTUSB_CHANGE_BITS);
            io_mfence();
        }
        else
            retval = -1;
    }
    // kdebug("reset ok!");
    // 如果usb2端口成功reset，则处理该端口的active状态
    if (retval == 0 && XHCI_PORT_IS_USB2(id, port))
    {
        xhci_hc[id].ports[port].flags |= XHCI_PROTOCOL_ACTIVE;
        if (XHCI_PORT_HAS_PAIR(id, port)) // 如果有对应的usb3端口，则将usb3端口设置为未激活
            xhci_hc[id].ports[xhci_hc[id].ports[port].paired_port_num].flags &= ~(XHCI_PROTOCOL_ACTIVE);
    }

    // 如果usb3端口reset失败，则启用与之配对的usb2端口
    if (retval != 0 && XHCI_PORT_IS_USB3(id, port))
    {
        xhci_hc[id].ports[port].flags &= ~XHCI_PROTOCOL_ACTIVE;
        xhci_hc[id].ports[xhci_hc[id].ports[port].paired_port_num].flags |= XHCI_PROTOCOL_ACTIVE;
    }

    return retval;
}

/**
 * @brief 初始化设备slot的上下文，并将其写入dcbaap中的上下文index数组
 * - at this time, we don't know if the device is a hub or not, so we don't
 *   set the slot->hub, ->mtt, ->ttt, ->etc, items.
 *
 * @param id 控制器id
 * @param slot_id enable_slot命令分配的插槽id
 * @param port 端口号
 * @param speed 端口速度
 * @param max_packet 最大数据包大小
 * @return uint64_t 初始化好的设备上下文空间的虚拟地址
 */
static uint64_t xhci_initialize_slot(const int id, const int slot_id, const int port, const int speed, const int max_packet)
{
    // 暂时只初始化slot和control EP0

    // 申请上下文数据结构所占用的内存空间

    uint64_t device_context_vaddr = (uint64_t)kzalloc(xhci_hc[id].context_size * 2, 0);
    // kdebug("slot id=%d, device_context_vaddr=%#018lx, port=%d", slot_id, device_context_vaddr, port);
    // 写到数组中
    __write8b(xhci_hc[id].dcbaap_vaddr + (slot_id * sizeof(uint64_t)), virt_2_phys(device_context_vaddr));

    slot_ctx.entries = 1;
    slot_ctx.speed = speed;
    slot_ctx.route_string = 0;
    slot_ctx.rh_port_num = port + 1; // 由于xhci控制器是1-base的，因此把驱动程序中存储的端口号加1，才是真实的端口号
    slot_ctx.max_exit_latency = 0;   // 稍后会计算这个值
    slot_ctx.int_target = 0;         // 当前全部使用第0个interrupter
    slot_ctx.slot_state = XHCI_SLOT_STATE_DISABLED_OR_ENABLED;
    slot_ctx.device_address = 0;

    // 将slot信息写入上下文空间
    __write_slot(device_context_vaddr, &slot_ctx);

    xhci_initialize_ep(id, device_context_vaddr, slot_id, XHCI_EP_CONTROL, max_packet, USB_EP_CONTROL, 0, speed, 0);

    return device_context_vaddr;
}

/**
 * @brief 初始化endpoint
 *
 * @param id 控制器id
 * @param slot_vaddr slot上下文的虚拟地址
 * @param slot_id 插槽id
 * @param ep_num 端点上下文在slot上下文区域内的编号
 * @param max_packet 最大数据包大小
 * @param type 端点类型
 * @param direction 传输方向
 * @param speed 传输速度
 * @param ep_interval 端点的连续请求间隔
 */
static void xhci_initialize_ep(const int id, const uint64_t slot_vaddr, const int slot_id, const int ep_num, const int max_packet, const int type, const int direction, const int speed, const int ep_interval)
{
    // 由于目前只实现获取设备的描述符，因此暂时只支持control ep
    if (type != USB_EP_CONTROL)
        return;
    memset(&ep_ctx, 0, sizeof(struct xhci_ep_context_t));

    xhci_hc[id].control_ep_info.ep_ring_vbase = xhci_create_ring(XHCI_TRBS_PER_RING);
    // 申请ep的 transfer ring
    ep_ctx.tr_dequeue_ptr = virt_2_phys(xhci_hc[id].control_ep_info.ep_ring_vbase);
    xhci_ep_set_dequeue_cycle_state(&ep_ctx, XHCI_TRB_CYCLE_ON);

    xhci_hc[id].control_ep_info.current_ep_ring_vaddr = xhci_hc[id].control_ep_info.ep_ring_vbase;
    xhci_hc[id].control_ep_info.current_ep_ring_cycle = xhci_ep_get_dequeue_cycle_state(&ep_ctx);
    // kdebug("ep_ctx.tr_dequeue_ptr = %#018lx", ep_ctx.tr_dequeue_ptr);
    // kdebug("xhci_hc[id].control_ep_info.current_ep_ring_cycle  = %d", xhci_hc[id].control_ep_info.current_ep_ring_cycle);

    // 设置初始值
    ep_ctx.max_packet_size = max_packet;
    ep_ctx.linear_stream_array = 0;
    ep_ctx.max_primary_streams = 0;
    ep_ctx.mult = 0;
    ep_ctx.ep_state = XHCI_EP_STATE_DISABLED;
    ep_ctx.hid = 0;
    ep_ctx.ep_type = 4;
    ep_ctx.average_trb_len = 8; // 所有的control ep的该值均为8
    ep_ctx.err_cnt = 3;
    ep_ctx.max_burst_size = 0;
    ep_ctx.interval = ep_interval;
    // 将ep的信息写入到slot上下文中对应的ep的块中
    __write_ep(id, slot_vaddr, ep_num, &ep_ctx);
}

/**
 * @brief 向usb控制器发送 address_device命令
 *
 * @param id 主机控制器id
 * @param slot_vaddr 插槽上下文的虚拟基地址
 * @param slot_id 插槽id
 * @param block 是否阻断 set address 信息向usb设备的传输
 * @return int 错误码
 */
static int xhci_set_address(const int id, const uint64_t slot_vaddr, const int slot_id, const bool block)
{
    int retval = 0;
    // 创建输入上下文缓冲区
    uint64_t input_ctx_buffer = (uint64_t)kzalloc(xhci_hc[id].context_size * 32, 0);

    // 置位input control context和slot context的add bit
    __write4b(input_ctx_buffer + 4, 0x3);

    // 拷贝slot上下文和control ep上下文到输入上下文中
    __write_slot(input_ctx_buffer + xhci_hc[id].context_size, (struct xhci_slot_context_t *)slot_vaddr);

    __write_ep(id, input_ctx_buffer, 2, (struct xhci_ep_context_t *)(slot_vaddr + XHCI_EP_CONTROL * xhci_hc[id].context_size));

    struct xhci_TRB_normal_t trb = {0};
    trb.buf_paddr = virt_2_phys(input_ctx_buffer);
    trb.bei = (block ? 1 : 0);
    trb.TRB_type = TRB_TYPE_ADDRESS_DEVICE;
    trb.intr_target = 0;
    trb.cycle = xhci_hc[id].cmd_trb_cycle;
    trb.Reserved |= ((slot_id << 8) & 0xffff);

    retval = xhci_send_command(id, (struct xhci_TRB_t *)&trb, true);
    if (unlikely(retval != 0))
    {
        kerror("slotid:%d, address device failed", slot_id);

        goto failed;
    }

    struct xhci_TRB_cmd_complete_t *trb_done = (struct xhci_TRB_cmd_complete_t *)&trb;
    // kdebug("address slot: comp code=%d", trb_done->code);
    if (trb_done->code == TRB_COMP_TRB_SUCCESS) // 成功执行
    {
        // 如果要从控制器获取刚刚设置的设备地址的话，可以在这里读取slot context
        // ksuccess("slot %d successfully addressed.", slot_id);
        struct xhci_slot_context_t slot;
        struct xhci_ep_context_t ep;
        __read_from_slot(&slot, slot_vaddr);
        slot_ctx.slot_state = slot.slot_state;
        slot_ctx.device_address = slot.device_address;
        __read_from_ep(id, slot_vaddr, 1, &ep);
        // kdebug("ep.ep_state=%d, slot_state=%d", ep.ep_state, slot.slot_state);
        ep_ctx.ep_state = ep.ep_state;
        ep_ctx.max_packet_size = ep.max_packet_size;
        retval = 0;
    }
    else
        retval = -EAGAIN;
done:;
failed:;
    kfree((void *)input_ctx_buffer);
    return retval;
}

/**
 * @brief 在指定的端点的ring中，写入一个setup stage TRB
 *
 * @param ep 端点信息结构体
 * @param packet usb请求包
 * @param direction 传输的方向
 * @return int 产生的TRB数量
 */
static int xhci_setup_stage(struct xhci_ep_ring_info_t *ep, const struct usb_request_packet_t *packet, const uint8_t direction)
{
    // kdebug("ep->current_ep_ring_cycle=%d", ep->current_ep_ring_cycle);
    struct xhci_TRB_setup_stage_t trb = {0};
    trb.bmRequestType = packet->request_type;
    trb.bRequest = packet->request;
    trb.wValue = packet->value;
    trb.wIndex = packet->index;
    trb.wLength = packet->length;
    trb.transfer_legth = 8;
    trb.intr_target = 0; // 使用第0个interrupter
    trb.cycle = ep->current_ep_ring_cycle;
    trb.ioc = 0;
    trb.idt = 1;
    trb.TRB_type = TRB_TYPE_SETUP_STAGE;
    trb.trt = direction;

    // 将setup stage trb拷贝到ep的transfer ring中
    __xhci_write_trb(ep, (struct xhci_TRB_t *)&trb);
    return 1;
}

/**
 * @brief 向指定的端点中写入data stage trb
 *
 * @param ep 端点信息结构体
 * @param buf_vaddr 数据缓冲区虚拟地址
 * @param trb_type trb类型
 * @param size 要传输的数据大小
 * @param direction 传输方向
 * @param max_packet 最大请求包大小
 * @param status_vaddr event data TRB的缓冲区（4字节，且地址按照16字节对齐）
 * @return int 产生的TRB数量
 */
static int xhci_data_stage(struct xhci_ep_ring_info_t *ep, uint64_t buf_vaddr, uint8_t trb_type, const uint32_t size, uint8_t direction, const int max_packet, const uint64_t status_vaddr)
{
    if (size == 0)
        return 0;
    int64_t remain_bytes = size;
    uint32_t remain_packets = (size + max_packet - 1) / max_packet;
    struct xhci_TRB_data_stage_t trb = {0};
    int count_packets = 0;
    // 分多个trb来执行
    while (remain_bytes > 0)
    {
        --remain_packets;

        trb.buf_paddr = virt_2_phys(buf_vaddr);
        trb.intr_target = 0;
        trb.TD_size = remain_packets;
        trb.transfer_length = (remain_bytes < max_packet ? size : max_packet);
        trb.dir = direction;
        trb.TRB_type = trb_type;
        trb.chain = 1;
        trb.ent = (remain_packets == 0);
        trb.cycle = ep->current_ep_ring_cycle;
        trb.ioc = 0;

        // 将data stage trb拷贝到ep的transfer ring中
        __xhci_write_trb(ep, (struct xhci_TRB_t *)&trb);

        buf_vaddr += max_packet;
        remain_bytes -= max_packet;
        ++count_packets;

        // 对于data stage trb而言，除了第一个trb以外，剩下的trb都是NORMAL的，并且dir是无用的
        trb_type = TRB_TYPE_NORMAL;
        direction = 0;
    }

    // 写入data event trb, 待完成后，完成信息将会存到status_vaddr指向的地址中
    memset(&trb, 0, sizeof(struct xhci_TRB_data_stage_t *));
    trb.buf_paddr = virt_2_phys(status_vaddr);
    trb.intr_target = 0;
    trb.cycle = ep->current_ep_ring_cycle;
    trb.ioc = 1;
    trb.TRB_type = TRB_TYPE_EVENT_DATA;
    __xhci_write_trb(ep, (struct xhci_TRB_t *)&trb);

    return count_packets + 1;
}

/**
 * @brief 填写xhci status stage TRB到control ep的transfer ring
 *
 * @param id 主机控制器id
 * @param direction 方向：（h2d:0, d2h:1）
 * @param status_buf_vaddr
 * @return int 创建的TRB数量
 */
static int xhci_status_stage(const int id, uint8_t direction, uint64_t status_buf_vaddr)
{
    // kdebug("write status stage trb");
    {
        struct xhci_TRB_status_stage_t trb = {0};

        // 写入status stage trb
        trb.intr_target = 0;
        trb.cycle = xhci_hc[id].control_ep_info.current_ep_ring_cycle;
        trb.ent = 0;
        trb.ioc = 1;
        trb.TRB_type = TRB_TYPE_STATUS_STAGE;
        trb.dir = direction;
        __xhci_write_trb(&xhci_hc[id].control_ep_info, (struct xhci_TRB_t *)&trb);
    }

    {
        // 写入event data TRB
        struct xhci_TRB_data_stage_t trb = {0};
        trb.buf_paddr = virt_2_phys(status_buf_vaddr);
        trb.intr_target = 0;
        trb.TRB_type = TRB_TYPE_EVENT_DATA;
        trb.ioc = 1;

        trb.cycle = xhci_hc[id].control_ep_info.current_ep_ring_cycle;

        __xhci_write_trb(&xhci_hc[id].control_ep_info, (struct xhci_TRB_t *)&trb);
    }
    return 2;
}

/**
 * @brief 等待状态数据被拷贝到status缓冲区中
 *
 * @param id 主机控制器id
 * @param status_vaddr status 缓冲区
 * @return int 错误码
 */
static int xhci_wait_for_interrupt(const int id, uint64_t status_vaddr)
{
    int timer = 500;
    while (timer)
    {
        if (__read4b(status_vaddr) & XHCI_IRQ_DONE)
        {
            uint32_t status = __read4b(status_vaddr);
            // 判断完成码
            switch (xhci_get_comp_code(status))
            {
            case TRB_COMP_TRB_SUCCESS:
            case TRB_COMP_SHORT_PACKET:
                return 0;
                break;
            case TRB_COMP_STALL_ERROR:
            case TRB_COMP_DATA_BUFFER_ERROR:
            case TRB_COMP_BABBLE_DETECTION:
                return -EINVAL;
            default:
                kerror("xhci wait interrupt: status=%#010x, complete_code=%d", status, xhci_get_comp_code(status));
                return -EIO;
            }
        }
        --timer;
        usleep(1000);
    }

    kerror(" USB xHCI Interrupt wait timed out.");
    return -ETIMEDOUT;
}

/**
 * @brief 从指定插槽的control endpoint读取信息
 *
 * @param id 主机控制器id
 * @param target 读取到的信息存放到的位置
 * @param in_size 要读取的数据的大小
 * @param slot_id 插槽id
 * @param max_packet 最大数据包大小
 * @return int 读取到的数据的大小
 */
static int xhci_control_in(const int id, void *target, const int in_size, const int slot_id, const int max_packet)
{

    uint64_t status_buf_vaddr = (uint64_t)kzalloc(16, 0); // 本来是要申请4bytes的buffer的，但是因为xhci控制器需要16bytes对齐，因此申请16bytes
    uint64_t data_buf_vaddr = (uint64_t)kzalloc(256, 0);
    int retval = 0;
    struct usb_request_packet_t packet = {0};
    packet.request_type = USB_REQ_TYPE_GET_REQUEST;
    packet.request = USB_REQ_GET_DESCRIPTOR;
    packet.value = (USB_DT_DEVICE << 8);
    packet.length = in_size;

    // 往control ep写入一个setup stage trb
    xhci_setup_stage(&xhci_hc[id].control_ep_info, &packet, XHCI_DIR_IN);
    xhci_data_stage(&xhci_hc[id].control_ep_info, data_buf_vaddr, TRB_TYPE_DATA_STAGE, in_size, XHCI_DIR_IN_BIT, max_packet, status_buf_vaddr);

/*
    QEMU doesn't quite handle SETUP/DATA/STATUS transactions correctly.
    It will wait for the STATUS TRB before it completes the transfer.
    Technically, you need to check for a good transfer before you send the
    STATUS TRB.  However, since QEMU doesn't update the status until after
    the STATUS TRB, waiting here will not complete a successful transfer.
    Bochs and real hardware handles this correctly, however QEMU does not.
    If you are using QEMU, do not ring the doorbell here.  Ring the doorbell
    *after* you place the STATUS TRB on the ring.
    (See bug report: https://bugs.launchpad.net/qemu/+bug/1859378 )
*/
#ifndef __QEMU_EMULATION__
    // 如果不是qemu虚拟机，则可以直接发起传输
    // kdebug(" not qemu");
    __xhci_write_doorbell(id, slot_id, XHCI_EP_CONTROL);
    retval = xhci_wait_for_interrupt(id, status_buf_vaddr);
    if (unlikely(retval != 0))
        goto failed;
#endif
    memset((void *)status_buf_vaddr, 0, 16);
    xhci_status_stage(id, XHCI_DIR_OUT_BIT, status_buf_vaddr);

    __xhci_write_doorbell(id, slot_id, XHCI_EP_CONTROL);

    retval = xhci_wait_for_interrupt(id, status_buf_vaddr);

    if (unlikely(retval != 0))
        goto failed;

    // 将读取到的数据拷贝到目标区域
    memcpy(target, (void *)data_buf_vaddr, in_size);
    retval = in_size;
    goto done;

failed:;
    kdebug("wait 4 interrupt failed");
    retval = 0;
done:;
    // 释放内存
    kfree((void *)status_buf_vaddr);
    kfree((void *)data_buf_vaddr);
    return retval;
}

/**
 * @brief 获取端口的描述信息
 *
 * @param id 主机控制器id
 * @param port_id 端口id
 * @return int 错误码
 */
static int xhci_get_descriptor(const int id, const int port_id)
{
    int retval = 0;
    int count = 0;
    struct usb_device_desc dev_desc = {0};
    uint32_t dword;
    // 计算port register set相对于operational registers基地址的偏移量
    uint32_t port_register_offset = XHCI_OPS_PRS + 16 * port_id;

    // 读取指定端口的port sc寄存器
    dword = xhci_read_op_reg32(id, port_register_offset + XHCI_PORT_PORTSC);

    // 读取端口速度。 full=1, low=2, high=3, super=4
    uint32_t speed = ((dword >> 10) & 0xf);

    /*
     * Some devices will only send the first 8 bytes of the device descriptor
     *  while in the default state.  We must request the first 8 bytes, then reset
     *  the port, set address, then request all 18 bytes.
     */
    struct xhci_TRB_normal_t trb = {0};
    trb.TRB_type = TRB_TYPE_ENABLE_SLOT;
    if (xhci_send_command(id, (struct xhci_TRB_t *)&trb, true) != 0)
    {
        kerror("portid:%d: send enable slot failed", port_id);
        return -ETIMEDOUT;
    }

    uint32_t slot_id = ((struct xhci_TRB_cmd_complete_t *)&trb)->slot_id;
    int16_t max_packet;
    if (slot_id != 0) // slot id不为0时，是合法的slot id
    {
        // 为不同速度的设备确定最大的数据包大小
        switch (speed)
        {
        case XHCI_PORT_SPEED_LOW:
            max_packet = 8;
            break;
        case XHCI_PORT_SPEED_FULL:
        case XHCI_PORT_SPEED_HI:
            max_packet = 64;
            break;
        case XHCI_PORT_SPEED_SUPER:
            max_packet = 512;
            break;
        }
    }
    kdebug("to init slot");
    // 初始化接口的上下文
    uint64_t slot_vaddr = xhci_initialize_slot(id, slot_id, port_id, speed, max_packet);

    kdebug("to set address");
    // 发送 address_device命令
    retval = xhci_set_address(id, slot_vaddr, slot_id, true);
    if (retval != 0)
        return retval;

    kdebug("to ctrl in");
    // 发送用于 “get_descriptor” 的数据包。
    count = xhci_control_in(id, &dev_desc, 8, slot_id, max_packet);
    if (unlikely(count == 0))
        return -EAGAIN;

    /*
        TODO: if the dev_desc.max_packet was different than what we have as max_packet,
          you would need to change it here and in the slot context by doing a
          evaluate_slot_context call.
    */

    // 重置当前端口
    kdebug("to reset");
    xhci_reset_port(id, port_id);

    // 再次发送 set_address命令
    kdebug("to set addr again");
    retval = xhci_set_address(id, slot_vaddr, slot_id, false);
    if (retval != 0)
        return retval;

    kdebug("to ctrl in again");
    count = xhci_control_in(id, &dev_desc, 18, slot_id, max_packet);
    if (unlikely(count == 0))
        return -EAGAIN;

    // print the descriptor
    printk("  Found USB Device:\n"
           "                port: %i\n"
           "                 len: %i\n"
           "                type: %i\n"
           "             version: %01X.%02X\n"
           "               class: %i\n"
           "            subclass: %i\n"
           "            protocol: %i\n"
           "     max packet size: %i\n"
           "           vendor id: 0x%04X\n"
           "          product id: 0x%04X\n"
           "         release ver: %i%i.%i%i\n"
           "   manufacture index: %i (index to a string)\n"
           "       product index: %i\n"
           "        serial index: %i\n"
           "   number of configs: %i\n",
           port_id, dev_desc.len, dev_desc.type, dev_desc.usb_version >> 8, dev_desc.usb_version & 0xFF, dev_desc._class, dev_desc.subclass,
           dev_desc.protocol, dev_desc.max_packet_size, dev_desc.vendor_id, dev_desc.product_id,
           (dev_desc.device_rel & 0xF000) >> 12, (dev_desc.device_rel & 0x0F00) >> 8,
           (dev_desc.device_rel & 0x00F0) >> 4, (dev_desc.device_rel & 0x000F) >> 0,
           dev_desc.manufacturer_index, dev_desc.procuct_index, dev_desc.serial_index, dev_desc.config);

    return 0;
}

/**
 * @brief 启用xhci控制器的端口
 *
 * @param id 控制器id
 * @return int
 */
static int xhci_hc_start_ports(int id)
{
    int cnt = 0;
    // 注意，这两个循环应该不能合并到一起，因为可能存在usb2端口offset在前，usb3端口在后的情况，那样的话就会出错

    // 循环启动所有的usb3端口
    // for (int i = 0; i < xhci_hc[id].port_num; ++i)
    for (int i = 0; i < 1; ++i)
    {
        if (XHCI_PORT_IS_USB3(id, i) && XHCI_PORT_IS_ACTIVE(id, i))
        {
            io_mfence();
            // reset该端口
            if (likely(xhci_reset_port(id, i) == 0)) // 如果端口reset成功，就获取它的描述符
                                                     // 否则，reset函数会把它给设置为未激活，并且标志配对的usb2端口是激活的
            {
                if (xhci_get_descriptor(id, i) == 0)
                    ++cnt;
            }
        }
    }
    kdebug("Active usb3 ports:%d", cnt);

    // 循环启动所有的usb2端口
    for (int i = 0; i < xhci_hc[id].port_num; ++i)
    {
        if (XHCI_PORT_IS_USB2(id, i) && XHCI_PORT_IS_ACTIVE(id, i))
        {
            // kdebug("initializing usb2: %d", i);
            // reset该端口
            if (likely(xhci_reset_port(id, i) == 0)) // 如果端口reset成功，就获取它的描述符
                                                     // 否则，reset函数会把它给设置为未激活，并且标志配对的usb2端口是激活的
            {
                // kdebug("reset ok");
                if (xhci_get_descriptor(id, i) == 0)
                    ++cnt;
                else
                    break;
            }
        }
    }
    kinfo("xHCI controller %d: Started %d ports.", id, cnt);
    return 0;
}

/**
 * @brief 初始化xhci主机控制器的中断控制
 *
 * @param id 主机控制器id
 * @return int 返回码
 */
static int xhci_hc_init_intr(int id)
{
    uint64_t retval = 0;

    struct xhci_caps_HCSPARAMS1_reg_t hcs1;
    struct xhci_caps_HCSPARAMS2_reg_t hcs2;
    io_mfence();
    memcpy(&hcs1, xhci_get_ptr_cap_reg32(id, XHCI_CAPS_HCSPARAMS1), sizeof(struct xhci_caps_HCSPARAMS1_reg_t));
    io_mfence();
    memcpy(&hcs2, xhci_get_ptr_cap_reg32(id, XHCI_CAPS_HCSPARAMS2), sizeof(struct xhci_caps_HCSPARAMS2_reg_t));
    io_mfence();

    uint32_t max_segs = (1 << (uint32_t)(hcs2.ERST_Max));
    uint32_t max_interrupters = hcs1.max_intrs;

    // 创建 event ring
    retval = xhci_create_event_ring(4096, &xhci_hc[id].event_ring_vaddr);
    io_mfence();
    if (unlikely((int64_t)(retval) == -ENOMEM))
        return -ENOMEM;
    xhci_hc[id].event_ring_table_vaddr = retval;
    xhci_hc[id].current_event_ring_vaddr = xhci_hc[id].event_ring_vaddr; // 设置驱动程序要读取的下一个event ring trb的地址
    retval = 0;

    xhci_hc[id].current_event_ring_cycle = 1;

    // 写入第0个中断寄存器组
    io_mfence();
    xhci_write_intr_reg32(id, 0, XHCI_IR_MAN, 0x3); // 使能中断并清除pending位（这个pending位是写入1就清0的）
    io_mfence();
    xhci_write_intr_reg32(id, 0, XHCI_IR_MOD, 0); // 关闭中断管制
    io_mfence();
    xhci_write_intr_reg32(id, 0, XHCI_IR_TABLE_SIZE, 1); // 当前只有1个segment
    io_mfence();
    xhci_write_intr_reg64(id, 0, XHCI_IR_DEQUEUE, virt_2_phys(xhci_hc[id].current_event_ring_vaddr) | (1 << 3)); // 写入dequeue寄存器，并清除busy位（写1就会清除）
    io_mfence();
    xhci_write_intr_reg64(id, 0, XHCI_IR_TABLE_ADDR, virt_2_phys(xhci_hc[id].event_ring_table_vaddr)); // 写入table地址
    io_mfence();

    // 清除状态位
    xhci_write_op_reg32(id, XHCI_OPS_USBSTS, (1 << 10) | (1 << 4) | (1 << 3) | (1 << 2));
    io_mfence();
    // 开启usb中断
    // 注册中断处理程序
    struct xhci_hc_irq_install_info_t install_info;
    install_info.assert = 1;
    install_info.edge_trigger = 1;
    install_info.processor = 0; // 投递到bsp

    char *buf = (char *)kmalloc(16, 0);
    memset(buf, 0, 16);
    sprintk(buf, "xHCI HC%d", id);
    io_mfence();
    irq_register(xhci_controller_irq_num[id], &install_info, &xhci_hc_irq_handler, id, &xhci_hc_intr_controller, buf);
    io_mfence();
    kfree(buf);

    kdebug("xhci host controller %d: interrupt registered. irq num=%d", id, xhci_controller_irq_num[id]);

    return 0;
}

/**
 * @brief 往xhci控制器发送trb, 并将返回的数据存入原始的trb中
 *
 * @param id xhci控制器号
 * @param trb 传输请求块
 * @param do_ring 是否通知doorbell register
 * @return int 错误码
 */
static int xhci_send_command(int id, struct xhci_TRB_t *trb, const bool do_ring)
{
    uint64_t origin_trb_vaddr = xhci_hc[id].cmd_trb_vaddr;

    // 必须先写入参数和状态数据，最后写入command
    __write8b(xhci_hc[id].cmd_trb_vaddr, trb->param);                                    // 参数
    __write4b(xhci_hc[id].cmd_trb_vaddr + 8, trb->status);                               // 状态
    __write4b(xhci_hc[id].cmd_trb_vaddr + 12, trb->command | xhci_hc[id].cmd_trb_cycle); // 命令

    xhci_hc[id].cmd_trb_vaddr += sizeof(struct xhci_TRB_t); // 跳转到下一个trb

    {
        // 如果下一个trb是link trb,则将下一个要操作的地址是设置为第一个trb
        struct xhci_TRB_normal_t *ptr = (struct xhci_TRB_normal_t *)xhci_hc[id].cmd_trb_vaddr;
        if (ptr->TRB_type == TRB_TYPE_LINK)
        {
            ptr->cycle = xhci_hc[id].cmd_trb_cycle;
            xhci_hc[id].cmd_trb_vaddr = xhci_hc[id].cmd_ring_vaddr;
            xhci_hc[id].cmd_trb_cycle ^= 1;
        }
    }

    if (do_ring) // 按响命令门铃
    {
        __xhci_write_doorbell(id, 0, 0);

        // 等待中断产生
        int timer = 400;
        const uint32_t iman0 = xhci_read_intr_reg32(id, 0, XHCI_IR_MAN);

        // Now wait for the interrupt to happen
        // We use bit 31 of the command dword since it is reserved
        while (timer && ((__read4b(origin_trb_vaddr + 8) & XHCI_IRQ_DONE) == 0))
        {
            usleep(1000);
            --timer;
        }
        uint32_t x = xhci_read_cap_reg32(id, xhci_hc[id].rts_offset + 0x20);
        if (timer == 0)
            return -ETIMEDOUT;
        else
        {
            xhci_get_trb(trb, origin_trb_vaddr);
            trb->status &= (~XHCI_IRQ_DONE);
        }
    }
    return 0;
}

/**
 * @brief 初始化xhci控制器
 *
 * @param header 指定控制器的pci device头部
 */
void xhci_init(struct pci_device_structure_general_device_t *dev_hdr)
{

    if (xhci_ctrl_count >= XHCI_MAX_HOST_CONTROLLERS)
    {
        kerror("Initialize xhci controller failed: exceed the limit of max controllers.");
        return;
    }

    spin_lock(&xhci_controller_init_lock);
    kinfo("Initializing xhci host controller: bus=%#02x, device=%#02x, func=%#02x, VendorID=%#04x, irq_line=%d, irq_pin=%d", dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, dev_hdr->header.Vendor_ID, dev_hdr->Interrupt_Line, dev_hdr->Interrupt_PIN);
    io_mfence();
    int cid = xhci_hc_find_available_id();
    if (cid < 0)
    {
        kerror("Initialize xhci controller failed: exceed the limit of max controllers.");
        goto failed_exceed_max;
    }

    memset(&xhci_hc[cid], 0, sizeof(struct xhci_host_controller_t));
    xhci_hc[cid].controller_id = cid;
    xhci_hc[cid].pci_dev_hdr = dev_hdr;
    io_mfence();
    {
        uint32_t tmp = pci_read_config(dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, 0x4);
        tmp |= 0x6;
        // mem I/O access enable and bus master enable
        pci_write_config(dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, 0x4, tmp);
    }
    io_mfence();
    // 为当前控制器映射寄存器地址空间
    xhci_hc[cid].vbase = SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + XHCI_MAPPING_OFFSET + 65536 * xhci_hc[cid].controller_id;
    // kdebug("dev_hdr->BAR0 & (~0xf)=%#018lx", dev_hdr->BAR0 & (~0xf));
    mm_map_phys_addr(xhci_hc[cid].vbase, dev_hdr->BAR0 & (~0xf), 65536, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD, true);
    io_mfence();

    // 计算operational registers的地址
    xhci_hc[cid].vbase_op = xhci_hc[cid].vbase + (xhci_read_cap_reg32(cid, XHCI_CAPS_CAPLENGTH) & 0xff);
    io_mfence();
    // 重置xhci控制器
    FAIL_ON_TO(xhci_hc_reset(cid), failed);
    io_mfence();

    // 读取xhci控制寄存器
    uint16_t iversion = *(uint16_t *)(xhci_hc[cid].vbase + XHCI_CAPS_HCIVERSION);

    struct xhci_caps_HCCPARAMS1_reg_t hcc1;
    struct xhci_caps_HCCPARAMS2_reg_t hcc2;

    struct xhci_caps_HCSPARAMS1_reg_t hcs1;
    struct xhci_caps_HCSPARAMS2_reg_t hcs2;
    memcpy(&hcc1, xhci_get_ptr_cap_reg32(cid, XHCI_CAPS_HCCPARAMS1), sizeof(struct xhci_caps_HCCPARAMS1_reg_t));
    memcpy(&hcc2, xhci_get_ptr_cap_reg32(cid, XHCI_CAPS_HCCPARAMS2), sizeof(struct xhci_caps_HCCPARAMS2_reg_t));
    memcpy(&hcs1, xhci_get_ptr_cap_reg32(cid, XHCI_CAPS_HCSPARAMS1), sizeof(struct xhci_caps_HCSPARAMS1_reg_t));
    memcpy(&hcs2, xhci_get_ptr_cap_reg32(cid, XHCI_CAPS_HCSPARAMS2), sizeof(struct xhci_caps_HCSPARAMS2_reg_t));

    xhci_hc[cid].db_offset = xhci_read_cap_reg32(cid, XHCI_CAPS_DBOFF) & (~0x3); // bits [1:0] reserved
    io_mfence();
    xhci_hc[cid].rts_offset = xhci_read_cap_reg32(cid, XHCI_CAPS_RTSOFF) & (~0x1f); // bits [4:0] reserved.
    io_mfence();

    xhci_hc[cid].ext_caps_off = 1UL * (hcc1.xECP) * 4;
    xhci_hc[cid].context_size = (hcc1.csz) ? 64 : 32;

    if (iversion < 0x95)
        kwarn("Unsupported/Unknowned xHCI controller version: %#06x. This may cause unexpected behavior.", iversion);

    {

        // Write to the FLADJ register incase the BIOS didn't
        uint32_t tmp = pci_read_config(dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, 0x60);
        tmp |= (0x20 << 8);
        pci_write_config(dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, 0x60, tmp);
    }
    // if it is a Panther Point device, make sure sockets are xHCI controlled.
    if (((pci_read_config(dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, 0) & 0xffff) == 0x8086) &&
        (((pci_read_config(dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, 0) >> 16) & 0xffff) == 0x1E31) &&
        ((pci_read_config(dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, 8) & 0xff) == 4))
    {
        kdebug("Is a Panther Point device");
        pci_write_config(dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, 0xd8, 0xffffffff);
        pci_write_config(dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, 0xd0, 0xffffffff);
    }
    io_mfence();
    // 关闭legacy支持
    FAIL_ON_TO(xhci_hc_stop_legacy(cid), failed);
    io_mfence();

    // 端口配对
    FAIL_ON_TO(xhci_hc_pair_ports(cid), failed);
    io_mfence();

    // ========== 设置USB host controller =========
    // 获取页面大小
    xhci_hc[cid].page_size = (xhci_read_op_reg32(cid, XHCI_OPS_PAGESIZE) & 0xffff) << 12;
    io_mfence();
    // 获取设备上下文空间
    xhci_hc[cid].dcbaap_vaddr = (uint64_t)kzalloc(2048, 0); // 分配2KB的设备上下文地址数组空间

    io_mfence();
    // kdebug("dcbaap_vaddr=%#018lx", xhci_hc[cid].dcbaap_vaddr);
    if (unlikely(!xhci_is_aligned64(xhci_hc[cid].dcbaap_vaddr))) // 地址不是按照64byte对齐
    {
        kerror("dcbaap isn't 64 byte aligned.");
        goto failed_free_dyn;
    }
    // 写入dcbaap
    xhci_write_op_reg64(cid, XHCI_OPS_DCBAAP, virt_2_phys(xhci_hc[cid].dcbaap_vaddr));
    io_mfence();
    // 创建command ring
    xhci_hc[cid].cmd_ring_vaddr = xhci_create_ring(XHCI_CMND_RING_TRBS);
    xhci_hc[cid].cmd_trb_vaddr = xhci_hc[cid].cmd_ring_vaddr;

    if (unlikely(!xhci_is_aligned64(xhci_hc[cid].cmd_ring_vaddr))) // 地址不是按照64byte对齐
    {
        kerror("cmd ring isn't 64 byte aligned.");
        goto failed_free_dyn;
    }

    // 设置初始cycle bit为1
    xhci_hc[cid].cmd_trb_cycle = XHCI_TRB_CYCLE_ON;
    io_mfence();
    // 写入command ring控制寄存器
    xhci_write_op_reg64(cid, XHCI_OPS_CRCR, virt_2_phys(xhci_hc[cid].cmd_ring_vaddr) | xhci_hc[cid].cmd_trb_cycle);
    // 写入配置寄存器
    uint32_t max_slots = hcs1.max_slots;
    // kdebug("max slots = %d", max_slots);
    io_mfence();
    xhci_write_op_reg32(cid, XHCI_OPS_CONFIG, max_slots);
    io_mfence();
    // 写入设备通知控制寄存器
    xhci_write_op_reg32(cid, XHCI_OPS_DNCTRL, (1 << 1)); // 目前只有N1被支持
    io_mfence();

    FAIL_ON_TO(xhci_hc_init_intr(cid), failed_free_dyn);
    io_mfence();

    ++xhci_ctrl_count;
    io_mfence();
    spin_unlock(&xhci_controller_init_lock);
    io_mfence();

    return;

failed_free_dyn:; // 释放动态申请的内存
    if (xhci_hc[cid].dcbaap_vaddr)
        kfree((void *)xhci_hc[cid].dcbaap_vaddr);

    if (xhci_hc[cid].cmd_ring_vaddr)
        kfree((void *)xhci_hc[cid].cmd_ring_vaddr);

    if (xhci_hc[cid].event_ring_table_vaddr)
        kfree((void *)xhci_hc[cid].event_ring_table_vaddr);

    if (xhci_hc[cid].event_ring_vaddr)
        kfree((void *)xhci_hc[cid].event_ring_vaddr);

failed:;
    io_mfence();
    // 取消地址映射
    mm_unmap_addr(xhci_hc[cid].vbase, 65536);
    io_mfence();
    // 清空数组
    memset((void *)&xhci_hc[cid], 0, sizeof(struct xhci_host_controller_t));

failed_exceed_max:;
    kerror("Failed to initialize controller: bus=%d, dev=%d, func=%d", dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func);
    spin_unlock(&xhci_controller_init_lock);
}