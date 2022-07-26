#include "xhci.h"
#include <common/kprint.h>
#include <debug/bug.h>
#include <process/spinlock.h>
#include <mm/mm.h>
#include <mm/slab.h>
#include <debug/traceback/traceback.h>
#include <common/time.h>
#include <exception/irq.h>
#include <driver/interrupt/apic/apic.h>

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

#define xhci_read_cap_reg8(id, offset) (*(uint8_t *)(xhci_hc[id].vbase + offset))
#define xhci_get_ptr_cap_reg8(id, offset) ((uint8_t *)(xhci_hc[id].vbase + offset))
#define xhci_write_cap_reg8(id, offset, value) (*(uint8_t *)(xhci_hc[id].vbase + offset) = (uint8_t)value)

#define xhci_read_cap_reg32(id, offset) (*(uint32_t *)(xhci_hc[id].vbase + offset))
#define xhci_get_ptr_cap_reg32(id, offset) ((uint32_t *)(xhci_hc[id].vbase + offset))
#define xhci_write_cap_reg32(id, offset, value) (*(uint32_t *)(xhci_hc[id].vbase + offset) = (uint32_t)value)

#define xhci_read_cap_reg64(id, offset) (*(uint64_t *)(xhci_hc[id].vbase + offset))
#define xhci_get_ptr_reg64(id, offset) ((uint64_t *)(xhci_hc[id].vbase + offset))
#define xhci_write_cap_reg64(id, offset, value) (*(uint64_t *)(xhci_hc[id].vbase + offset) = (uint64_t)value)

#define xhci_read_op_reg8(id, offset) (*(uint8_t *)(xhci_hc[id].vbase_op + offset))
#define xhci_get_ptr_op_reg8(id, offset) ((uint8_t *)(xhci_hc[id].vbase_op + offset))
#define xhci_write_op_reg8(id, offset, value) (*(uint8_t *)(xhci_hc[id].vbase_op + offset) = (uint8_t)value)

#define xhci_read_op_reg32(id, offset) (*(uint32_t *)(xhci_hc[id].vbase_op + offset))
#define xhci_get_ptr_op_reg32(id, offset) ((uint32_t *)(xhci_hc[id].vbase_op + offset))
#define xhci_write_op_reg32(id, offset, value) (*(uint32_t *)(xhci_hc[id].vbase_op + offset) = (uint32_t)value)

#define xhci_read_op_reg64(id, offset) (*(uint64_t *)(xhci_hc[id].vbase_op + offset))
#define xhci_get_ptr_op_reg64(id, offset) ((uint64_t *)(xhci_hc[id].vbase_op + offset))
#define xhci_write_op_reg64(id, offset, value) (*(uint64_t *)(xhci_hc[id].vbase_op + offset) = (uint64_t)value)

/**
 * @brief 计算中断寄存器组虚拟地址
 * @param id 主机控制器id
 * @param num xhci中断寄存器组号
 */
#define xhci_calc_intr_vaddr(id, num) (xhci_hc[id].vbase + xhci_hc[id].rts_offset + XHCI_RT_IR0 + num * XHCI_IR_SIZE)
/**
 * @brief 读取/写入中断寄存器
 * @param id 主机控制器id
 * @param num xhci中断寄存器组号
 * @param intr_offset 寄存器在当前寄存器组中的偏移量
 */
#define xhci_read_intr_reg32(id, num, intr_offset) (*(uint32_t *)(xhci_calc_intr_vaddr(id, num) + intr_offset))
#define xhci_write_intr_reg32(id, num, intr_offset, value) (*(uint32_t *)(xhci_calc_intr_vaddr(id, num) + intr_offset) = value)
#define xhci_read_intr_reg64(id, num, intr_offset) (*(uint64_t *)(xhci_calc_intr_vaddr(id, num) + intr_offset))
#define xhci_write_intr_reg64(id, num, intr_offset, value) (*(uint64_t *)(xhci_calc_intr_vaddr(id, num) + intr_offset) = value)

#define xhci_is_aligned64(addr) ((addr & 0x3f) == 0) // 是否64bytes对齐

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
#define xhci_TRB_set_link_cmd(trb_vaddr)                                       \
    do                                                                         \
    {                                                                          \
        struct xhci_TRB_normal_t *ptr = (struct xhci_TRB_normal_t *)trb_vaddr; \
        ptr->TRB_type = TRB_TYPE_LINK;                                         \
        ptr->ioc = 0;                                                          \
        ptr->chain = 0;                                                        \
        ptr->ent = 0;                                                          \
        ptr->cycle = 1;                                                        \
    } while (0)

// Common TRB types
enum
{
    TRB_TYPE_NORMAL = 1,
    TRB_TYPE_SETUP_STAGE,
    TRB_TYPE_DATA_STAGE,
    TRB_TYPE_STATUS_STAGE,
    TRB_TYPE_ISOCH,
    TRB_TYPE_LINK,
    TRB_TYPE_EVENT_DATA,
    TRB_TYPE_NO_OP,
    TRB_TYPE_ENABLE_SLOT,
    TRB_TYPE_DISABLE_SLOT = 10,

    TRB_TYPE_ADDRESS_DEVICE = 11,
    TRB_TYPE_CONFIG_EP,
    TRB_TYPE_EVALUATE_CONTEXT,
    TRB_TYPE_RESET_EP,
    TRB_TYPE_STOP_EP = 15,
    TRB_TYPE_SET_TR_DEQUEUE,
    TRB_TYPE_RESET_DEVICE,
    TRB_TYPE_FORCE_EVENT,
    TRB_TYPE_DEG_BANDWIDTH,
    TRB_TYPE_SET_LAT_TOLERANCE = 20,

    TRB_TYPE_GET_PORT_BAND = 21,
    TRB_TYPE_FORCE_HEADER,
    TRB_TYPE_NO_OP_CMD, // 24 - 31 = reserved

    TRB_TYPE_TRANS_EVENT = 32,
    TRB_TYPE_COMMAND_COMPLETION,
    TRB_TYPE_PORT_STATUS_CHANGE,
    TRB_TYPE_BANDWIDTH_REQUEST,
    TRB_TYPE_DOORBELL_EVENT,
    TRB_TYPE_HOST_CONTROLLER_EVENT = 37,
    TRB_TYPE_DEVICE_NOTIFICATION,
    TRB_TYPE_MFINDEX_WRAP,
    // 40 - 47 = reserved
    // 48 - 63 = Vendor Defined
};

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

    xhci_write_op_reg32(id, XHCI_OPS_USBCMD, 0x00000000);
    char timeout = 17;
    while ((xhci_read_op_reg32(id, XHCI_OPS_USBSTS) & (1 << 0)) == 0)
    {
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
    kdebug("usbsts=%#010lx", xhci_read_op_reg32(id, XHCI_OPS_USBSTS));
    // 判断HCHalted是否置位
    if ((xhci_read_op_reg32(id, XHCI_OPS_USBSTS) & (1 << 0)) == 0)
    {
        kdebug("stopping usb hc...");
        // 未置位，需要先尝试停止usb主机控制器
        retval = xhci_hc_stop(id);
        if (unlikely(retval))
            return retval;
    }
    int timeout = 500; // wait 500ms
    // reset
    uint32_t cmd = xhci_read_op_reg32(id, XHCI_OPS_USBCMD);
    kdebug("cmd=%#010lx", cmd);
    cmd |= (1 << 1);
    xhci_write_op_reg32(id, XHCI_OPS_USBCMD, cmd);
    kdebug("after rst, sts=%#010lx", xhci_read_op_reg32(id, XHCI_OPS_USBSTS));
    while (xhci_read_op_reg32(id, XHCI_OPS_USBCMD) & (1 << 1))
    {
        usleep(1000);
        if (--timeout == 0)
            return -ETIMEDOUT;
    }
    // kdebug("reset done!, timeout=%d", timeout);
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
        if (xhci_read_cap_reg8(id, current_offset) == XHCI_XECP_ID_LEGACY)
        {

            // 接管控制权
            xhci_write_cap_reg32(id, current_offset, xhci_read_cap_reg32(id, current_offset) | XHCI_XECP_LEGACY_OS_OWNED);

            // 等待响应完成
            int timeout = XHCI_XECP_LEGACY_TIMEOUT;
            while ((xhci_read_cap_reg32(id, current_offset) & XHCI_XECP_LEGACY_OWNING_MASK) != XHCI_XECP_LEGACY_OS_OWNED)
            {
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

        // 读取下一个entry的偏移增加量
        int next_off = ((xhci_read_cap_reg32(id, current_offset) & 0xff00) >> 8) << 2;
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
    xhci_write_op_reg32(id, XHCI_OPS_USBCMD, (1 << 0) | (1 >> 2) | (1 << 3));
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
    xhci_write_op_reg32(id, XHCI_OPS_USBCMD, 0x00);
}

/**
 * @brief
 *
 * @return uint32_t
 */

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
        uint32_t next_list_off = (dw0 >> 8) & 0xff;
        next_list_off = next_list_off ? (list_off + (next_list_off << 2)) : 0;

        if ((dw0 & 0xff) == XHCI_XECP_ID_PROTOCOL && ((dw0 & 0xff000000) >> 24) == version)
        {
            uint32_t dw2 = xhci_read_cap_reg32(id, list_off + 8);

            if (offset != NULL)
                *offset = (uint32_t)(dw2 & 0xff) - 1; // 使其转换为zero based
            if (count != NULL)
                *count = (uint32_t)((dw2 & 0xff00) >> 8);
            if (protocol_flag != NULL)
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
    memcpy(&hcs1, xhci_get_ptr_cap_reg32(id, XHCI_CAPS_HCSPARAMS1), sizeof(struct xhci_caps_HCSPARAMS1_reg_t));

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
        next_off = xhci_hc_get_protocol_offset(id, next_off, 2, &offset, &cnt, &protocol_flags);

        if (cnt)
        {
            for (int i = 0; i < cnt; ++i)
            {
                xhci_hc[id].ports[offset + i].offset = xhci_hc[id].port_num_u2++;
                xhci_hc[id].ports[offset + i].flags = XHCI_PROTOCOL_USB2;

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
        next_off = xhci_hc_get_protocol_offset(id, next_off, 3, &offset, &cnt, &protocol_flags);

        if (cnt)
        {
            for (int i = 0; i < cnt; ++i)
            {
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

            if ((xhci_hc[id].ports[i].offset == xhci_hc[id].ports[j].offset) &&
                ((xhci_hc[id].ports[i].flags & XHCI_PROTOCOL_INFO) != (xhci_hc[id].ports[j].flags & XHCI_PROTOCOL_INFO)))
            {
                xhci_hc[id].ports[i].paired_port_num = j;
                xhci_hc[id].ports[i].flags |= XHCI_PROTOCOL_HAS_PAIR;

                xhci_hc[id].ports[j].paired_port_num = i;
                xhci_hc[id].ports[j].flags |= XHCI_PROTOCOL_HAS_PAIR;
            }
        }
    }

    // 标记所有的usb3、单独的usb2端口为激活状态
    for (int i = 0; i < xhci_hc[id].port_num; ++i)
    {
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
    memset((void *)vaddr, 0, total_size);

    // 设置最后一个trb为link trb
    xhci_TRB_set_link_cmd(vaddr + total_size - sizeof(struct xhci_TRB_t));

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
    if (unlikely(table_vaddr == NULL))
        return -ENOMEM;
    memset((void *)table_vaddr, 0, 64);

    // 暂时只创建1个segment
    const uint64_t seg_vaddr = (const uint64_t)kmalloc(trbs * sizeof(struct xhci_TRB_t), 0);

    if (unlikely(seg_vaddr == NULL))
        return -ENOMEM;

    memset((void *)seg_vaddr, 0, trbs * sizeof(struct xhci_TRB_t));

    // 将segment地址和大小写入table
    *(uint64_t *)(table_vaddr) = virt_2_phys(seg_vaddr);
    *(uint64_t *)(table_vaddr + 8) = trbs;

    *ret_ring_addr = seg_vaddr;
    return table_vaddr;
}

void xhci_hc_irq_enable(uint64_t irq_num)
{
    int cid = xhci_find_hcid_by_irq_num(irq_num);
    if (WARN_ON(cid == -1))
        return;
    kdebug("start msi");
    pci_start_msi(xhci_hc[cid].pci_dev_hdr);
    kdebug("start sched");
    xhci_hc_start_sched(cid);
    kdebug("start ports");
    xhci_hc_start_ports(cid);
    kdebug("enabled");
}

void xhci_hc_irq_disable(uint64_t irq_num)
{
    int cid = xhci_find_hcid_by_irq_num(irq_num);
    if (WARN_ON(cid == -1))
        return;

    xhci_hc_stop_sched(cid);
    pci_disable_msi(xhci_hc[cid].pci_dev_hdr);
}

uint64_t xhci_hc_irq_install(uint64_t irq_num, void *arg)
{
    int cid = xhci_find_hcid_by_irq_num(irq_num);
    if (WARN_ON(cid == -1))
        return -EINVAL;

    struct xhci_hc_irq_install_info_t *info = (struct xhci_hc_irq_install_info_t *)arg;
    struct msi_desc_t msi_desc;
    memset(&msi_desc, 0, sizeof(struct msi_desc_t));

    msi_desc.pci_dev = (struct pci_device_structure_header_t *)xhci_hc[cid].pci_dev_hdr;
    msi_desc.assert = info->assert;
    msi_desc.edge_trigger = info->edge_trigger;
    msi_desc.processor = info->processor;
    msi_desc.pci.msi_attribute.is_64 = 1;
    // todo: QEMU是使用msix的，因此要先在pci中实现msix
    int retval = pci_enable_msi(&msi_desc);
    kdebug("pci retval = %d", retval);
    kdebug("xhci irq %d installed.", irq_num);
    return 0;
}

void xhci_hc_irq_uninstall(uint64_t irq_num)
{
    // todo
    int cid = xhci_find_hcid_by_irq_num(irq_num);
    if (WARN_ON(cid == -1))
        return;
    xhci_hc_stop(cid);
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
    // todo: handle irq
    kdebug("USB irq received.");
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
    // kdebug("to reset %d, offset=%#018lx", port, port_status_offset);
    // 检查端口电源状态
    if ((xhci_read_op_reg32(id, port_status_offset + XHCI_PORT_PORTSC) & (1 << 9)) == 0)
    {
        kdebug("port is power off, starting...");
        xhci_write_cap_reg32(id, port_status_offset + XHCI_PORT_PORTSC, (1 << 9));
        usleep(2000);
        // 检测端口是否被启用, 若未启用，则报错
        if ((xhci_read_op_reg32(id, port_status_offset + XHCI_PORT_PORTSC) & (1 << 9)) == 0)
        {
            kdebug("cannot power on %d", port);
            return -EAGAIN;
        }
    }
    // kdebug("port:%d, power check ok", port);

    // 确保端口的status被清0
    xhci_write_op_reg32(id, port_status_offset + XHCI_PORT_PORTSC, (1 << 9) | XHCI_PORTUSB_CHANGE_BITS);

    // 重置当前端口
    if (XHCI_PORT_IS_USB3(id, port))
        xhci_write_op_reg32(id, port_status_offset + XHCI_PORT_PORTSC, (1 << 9) | (1 << 31));
    else
        xhci_write_op_reg32(id, port_status_offset + XHCI_PORT_PORTSC, (1 << 9) | (1 << 4));

    retval = -ETIMEDOUT;

    // 等待portsc的port reset change位被置位，说明reset完成
    int timeout = 200;
    while (timeout)
    {
        uint32_t val = xhci_read_op_reg32(id, port_status_offset + XHCI_PORT_PORTSC);
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
        usleep(USB_TIME_RST_REC * 1000);
        uint32_t val = xhci_read_op_reg32(id, port_status_offset + XHCI_PORT_PORTSC);

        // 如果reset之后，enable bit仍然是1，那么说明reset成功
        if (val & (1 << 1))
        {
            // 清除status change bit
            xhci_write_op_reg32(id, port_status_offset + XHCI_PORT_PORTSC, (1 << 9) | XHCI_PORTUSB_CHANGE_BITS);
        }
        retval = 0;
    }

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
    for (int i = 0; i < xhci_hc[id].port_num; ++i)
    {
        if (XHCI_PORT_IS_USB3(id, i) && XHCI_PORT_IS_ACTIVE(id, i))
        {
            // reset该端口
            if (likely(xhci_reset_port(id, i) == 0)) // 如果端口reset成功，就获取它的描述符
                                                     // 否则，reset函数会把它给设置为未激活，并且标志配对的usb2端口是激活的
            {
                // xhci_hc_get_descriptor(id, i);
                ++cnt;
            }
        }
    }
    kdebug("active usb3 ports:%d", cnt);

    // 循环启动所有的usb2端口
    for (int i = 0; i < xhci_hc[id].port_num; ++i)
    {
        if (XHCI_PORT_IS_USB2(id, i) && XHCI_PORT_IS_ACTIVE(id, i))
        {
            // reset该端口
            if (likely(xhci_reset_port(id, i) == 0)) // 如果端口reset成功，就获取它的描述符
                                                     // 否则，reset函数会把它给设置为未激活，并且标志配对的usb2端口是激活的
            {
                // xhci_hc_get_descriptor(id, i);
                ++cnt;
            }
        }
    }
    kinfo("xHCI controller %d: Started %d ports.", id, cnt);
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
    memcpy(&hcs1, xhci_get_ptr_cap_reg32(id, XHCI_CAPS_HCSPARAMS1), sizeof(struct xhci_caps_HCSPARAMS1_reg_t));
    memcpy(&hcs2, xhci_get_ptr_cap_reg32(id, XHCI_CAPS_HCSPARAMS2), sizeof(struct xhci_caps_HCSPARAMS2_reg_t));

    uint32_t max_segs = (1 << (uint32_t)(hcs2.ERST_Max));
    uint32_t max_interrupters = hcs1.max_intrs;

    // 创建 event ring
    retval = xhci_create_event_ring(4096, &xhci_hc[id].event_ring_vaddr);
    if (unlikely((int64_t)(retval) == -ENOMEM))
        return -ENOMEM;
    xhci_hc[id].event_ring_table_vaddr = retval;
    retval = 0;

    xhci_hc[id].current_event_ring_cycle = 1;

    // 写入第0个中断寄存器组
    xhci_write_intr_reg32(id, 0, XHCI_IR_MAN, 0x3);                                                      // 使能中断并清除pending位（这个pending位是写入1就清0的）
    xhci_write_intr_reg32(id, 0, XHCI_IR_MOD, 0);                                                        // 关闭中断管制
    xhci_write_intr_reg32(id, 0, XHCI_IR_TABLE_SIZE, 1);                                                 // 当前只有1个segment
    xhci_write_intr_reg64(id, 0, XHCI_IR_DEQUEUE, virt_2_phys(xhci_hc[id].event_ring_vaddr) | (1 << 3)); // 写入dequeue寄存器，并清除busy位（写1就会清除）
    xhci_write_intr_reg64(id, 0, XHCI_IR_TABLE_ADDR, virt_2_phys(xhci_hc[id].event_ring_table_vaddr));   // 写入table地址

    // 清除状态位
    xhci_write_op_reg32(id, XHCI_OPS_USBSTS, (1 << 10) | (1 << 4) | (1 << 3) | (1 << 2));

    // 开启usb中断
    // 注册中断处理程序
    struct xhci_hc_irq_install_info_t install_info;
    install_info.assert = 1;
    install_info.edge_trigger = 1;
    install_info.processor = 0; // 投递到bsp

    char *buf = (char *)kmalloc(16, 0);
    memset(buf, 0, 16);
    sprintk(buf, "xHCI HC%d", id);
    irq_register(xhci_controller_irq_num[id], &install_info, &xhci_hc_irq_handler, id, &xhci_hc_intr_controller, buf);
    kfree(buf);

    kdebug("xhci host controller %d: interrupt registered. irq num=%d", id, xhci_controller_irq_num[id]);

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

    int cid = xhci_hc_find_available_id();
    if (cid < 0)
    {
        kerror("Initialize xhci controller failed: exceed the limit of max controllers.");
        goto failed_exceed_max;
    }

    memset(&xhci_hc[cid], 0, sizeof(struct xhci_host_controller_t));
    xhci_hc[cid].controller_id = cid;
    xhci_hc[cid].pci_dev_hdr = dev_hdr;
    pci_write_config(dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, 0x4, 0x0006); // mem I/O access enable and bus master enable

    // 为当前控制器映射寄存器地址空间
    xhci_hc[cid].vbase = SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + XHCI_MAPPING_OFFSET + 65536 * xhci_hc[cid].controller_id;
    // kdebug("dev_hdr->BAR0 & (~0xf)=%#018lx", dev_hdr->BAR0 & (~0xf));
    mm_map_phys_addr(xhci_hc[cid].vbase, dev_hdr->BAR0 & (~0xf), 65536, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD, true);

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

    // kdebug("hcc1.xECP=%#010lx", hcc1.xECP);
    // 计算operational registers的地址
    xhci_hc[cid].vbase_op = xhci_hc[cid].vbase + xhci_read_cap_reg8(cid, XHCI_CAPS_CAPLENGTH);

    xhci_hc[cid].db_offset = xhci_read_cap_reg32(cid, XHCI_CAPS_DBOFF) & (~0x3);    // bits [1:0] reserved
    xhci_hc[cid].rts_offset = xhci_read_cap_reg32(cid, XHCI_CAPS_RTSOFF) & (~0x1f); // bits [4:0] reserved.

    xhci_hc[cid].ext_caps_off = 1UL * (hcc1.xECP) * 4;
    xhci_hc[cid].context_size = (hcc1.csz) ? 64 : 32;

    if (iversion < 0x95)
    {
        kwarn("Unsupported/Unknowned xHCI controller version: %#06x. This may cause unexpected behavior.", iversion);
    }

    // if it is a Panther Point device, make sure sockets are xHCI controlled.
    if (((pci_read_config(dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, 0) & 0xffff) == 0x8086) &&
        ((pci_read_config(dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, 2) & 0xffff) == 0x1E31) &&
        ((pci_read_config(dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, 8) & 0xff) == 4))
    {
        kdebug("Is a Panther Point device");
        pci_write_config(dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, 0xd8, 0xffffffff);
        pci_write_config(dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, 0xd0, 0xffffffff);
    }

    // 关闭legacy支持
    FAIL_ON_TO(xhci_hc_stop_legacy(cid), failed);

    // 重置xhci控制器
    FAIL_ON_TO(xhci_hc_reset(cid), failed);
    // 端口配对
    FAIL_ON_TO(xhci_hc_pair_ports(cid), failed);

    // ========== 设置USB host controller =========
    // 获取页面大小
    kdebug("ops pgsize=%#010lx", xhci_read_op_reg32(cid, XHCI_OPS_PAGESIZE));
    xhci_hc[cid].page_size = (xhci_read_op_reg32(cid, XHCI_OPS_PAGESIZE) & 0xffff) << 12;
    kdebug("page size=%d", xhci_hc[cid].page_size);

    // 获取设备上下文空间
    xhci_hc[cid].dcbaap_vaddr = (uint64_t)kmalloc(2048, 0); // 分配2KB的设备上下文地址数组空间
    memset((void *)xhci_hc[cid].dcbaap_vaddr, 0, 2048);

    kdebug("dcbaap_vaddr=%#018lx", xhci_hc[cid].dcbaap_vaddr);
    if (unlikely(!xhci_is_aligned64(xhci_hc[cid].dcbaap_vaddr))) // 地址不是按照64byte对齐
    {
        kerror("dcbaap isn't 64 byte aligned.");
        goto failed_free_dyn;
    }
    // 写入dcbaap
    xhci_write_op_reg64(cid, XHCI_OPS_DCBAAP, virt_2_phys(xhci_hc[cid].dcbaap_vaddr));

    // 创建command ring
    xhci_hc[cid].cmd_ring_vaddr = xhci_create_ring(XHCI_CMND_RING_TRBS);
    if (unlikely(!xhci_is_aligned64(xhci_hc[cid].cmd_ring_vaddr))) // 地址不是按照64byte对齐
    {
        kerror("cmd ring isn't 64 byte aligned.");
        goto failed_free_dyn;
    }

    // 设置初始cycle bit为1
    xhci_hc[cid].cmd_trb_cycle = XHCI_TRB_CYCLE_ON;

    // 写入command ring控制寄存器
    xhci_write_op_reg64(cid, XHCI_OPS_CRCR, virt_2_phys(xhci_hc[cid].cmd_ring_vaddr) | xhci_hc[cid].cmd_trb_cycle);
    // 写入配置寄存器
    uint32_t max_slots = hcs1.max_slots;
    kdebug("max slots = %d", max_slots);
    xhci_write_op_reg32(cid, XHCI_OPS_CONFIG, max_slots);
    // 写入设备通知控制寄存器
    xhci_write_op_reg32(cid, XHCI_OPS_DNCTRL, (1 << 1)); // 目前只有N1被支持

    FAIL_ON_TO(xhci_hc_init_intr(cid), failed_free_dyn);
    ++xhci_ctrl_count;
    spin_unlock(&xhci_controller_init_lock);
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
    // 取消地址映射
    mm_unmap(xhci_hc[cid].vbase, 65536);

    // 清空数组
    memset((void *)&xhci_hc[cid], 0, sizeof(struct xhci_host_controller_t));

failed_exceed_max:;
    kerror("Failed to initialize controller: bus=%d, dev=%d, func=%d", dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func);
    spin_unlock(&xhci_controller_init_lock);
}