#include "xhci.h"
#include <common/kprint.h>
#include <debug/bug.h>
#include <process/spinlock.h>
#include <mm/mm.h>
#include <mm/slab.h>
#include <debug/traceback/traceback.h>
#include <common/time.h>

spinlock_t xhci_controller_init_lock; // xhci控制器初始化锁(在usb_init中被初始化)

static int xhci_ctrl_count = 0; // xhci控制器计数

static struct xhci_host_controller_t xhci_hc[XHCI_MAX_HOST_CONTROLLERS] = {0};

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

#define FAIL_ON(value, to)        \
    do                            \
    {                             \
        if (unlikely(value != 0)) \
            goto to;              \
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
    // 判断HCHalted是否置位
    if ((xhci_read_op_reg32(id, XHCI_OPS_USBSTS) & (1 << 0)) == 0)
    {
        // 未置位，需要先尝试停止usb主机控制器
        retval = xhci_hc_stop(id);
        if (unlikely(retval))
            return retval;
    }
    int timeout = 500; // wait 500ms
    // reset
    xhci_write_op_reg32(id, XHCI_OPS_USBCMD, (1 << 1));

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
                *offset = (uint32_t)(dw2 & 0xff);
            if (count != NULL)
                *count = (uint32_t)((dw2 & 0xff00) >> 8);
            if (protocol_flag != NULL)
                *protocol_flag = (uint16_t)((dw2 >> 16) & 0xffff);

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
    memcpy(&hcs1, xhci_get_ptr_cap_reg32(id, XHCI_CAPS_HCSPARAMS1), sizeof(struct xhci_caps_HCCPARAMS1_reg_t));

    // 从hcs1获取端口数量
    xhci_hc[id].port_num = hcs1.max_ports;

    // 找到所有的端口并标记其端口信息

    xhci_hc[id].port_num_u2 = 0;
    xhci_hc[id].port_num_u3 = 0;

    uint32_t next_off = xhci_hc[id].ext_caps_off;
    uint32_t offset, cnt;
    uint16_t protocol_flags;

    // 寻找所有的usb2端口
    while (next_off)
    {
        next_off = xhci_hc_get_protocol_offset(id, next_off, 2, &offset, &cnt, &protocol_flags);

        if (cnt)
        {
            for (int i = 0; i < cnt; ++i)
            {
                xhci_hc[id].ports[offset + i].offset = ++xhci_hc[id].port_num_u2;
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
                xhci_hc[id].ports[offset + i].offset = ++xhci_hc[id].port_num_u3;
                xhci_hc[id].ports[offset + i].flags = XHCI_PROTOCOL_USB3;
            }
        }
    }

    // 将对应的USB2端口和USB3端口进行配对
    for (int i = 1; i <= xhci_hc[id].port_num; ++i)
    {
        for (int j = i; j <= xhci_hc[id].port_num; ++j)
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

    // 标记所有的usb3端口为激活状态
    for (int i = 1; i <= xhci_hc[id].port_num; ++i)
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
                   XHCI_PORT_IS_ACTIVE(id, i) ? "active" : "inactive", XHCI_PORT_HAS_PAIR(id, i)?"true":"false");
        }
    }
    */

    return 0;
}

/**
 * @brief 创建ring，并将最后一个trb指向头一个trb
 *
 * @param trbs 要创建的trb数量
 * @return uint64_t
 */
static uint64_t xhci_create_ring(int trbs)
{
    int total_size = trbs * sizeof(struct xhci_TRB_t);
    const uint64_t vaddr = (uint64_t)kmalloc(total_size, 0);
    memset(vaddr, 0, total_size);

    // 设置最后一个trb为link trb
    xhci_TRB_set_link_cmd(vaddr + total_size - sizeof(sizeof(struct xhci_TRB_t)));

    return vaddr;
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
    memcpy(&hcc2, xhci_get_ptr_cap_reg32(cid, XHCI_CAPS_HCCPARAMS2), sizeof(struct xhci_caps_HCCPARAMS1_reg_t));
    memcpy(&hcs1, xhci_get_ptr_cap_reg32(cid, XHCI_CAPS_HCSPARAMS1), sizeof(struct xhci_caps_HCCPARAMS1_reg_t));
    memcpy(&hcs2, xhci_get_ptr_cap_reg32(cid, XHCI_CAPS_HCSPARAMS2), sizeof(struct xhci_caps_HCCPARAMS1_reg_t));

    // kdebug("hcc1.xECP=%#010lx", hcc1.xECP);
    // 计算operational registers的地址
    xhci_hc[cid].vbase_op = xhci_hc[cid].vbase + xhci_read_cap_reg8(cid, XHCI_CAPS_CAPLENGTH);

    xhci_hc[cid].db_offset = xhci_read_cap_reg32(cid, XHCI_CAPS_DBOFF) & (~0x3);    // bits [1:0] reserved
    xhci_hc[cid].rts_offset = xhci_read_cap_reg32(cid, XHCI_CAPS_RTSOFF) & (~0x1f); // bits [4:0] reserved.

    xhci_hc[cid].ext_caps_off = (hcc1.xECP) * 4;
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

    // 重置xhci控制器
    FAIL_ON(xhci_hc_reset(cid), failed);
    // 关闭legacy支持
    FAIL_ON(xhci_hc_stop_legacy(cid), failed);
    // 端口配对
    FAIL_ON(xhci_hc_pair_ports(cid), failed);

    // 获取页面大小
    xhci_hc[cid].page_size = (xhci_read_op_reg32(cid, XHCI_OPS_PAGESIZE) & 0xffff) << 12;
    kdebug("pg size=%d", xhci_hc[cid].page_size);

    // 获取设备上下文空间
    xhci_hc[cid].dcbaap_vaddr = (uint64_t)kmalloc(2048, 0); // 分配2KB的设备上下文地址数组空间
    memset(xhci_hc[cid].dcbaap_vaddr, 0, 2048);

    kdebug("dcbaap_vaddr=%#018lx", xhci_hc[cid].dcbaap_vaddr);
    if (unlikely(!xhci_is_aligned64(xhci_hc[cid].dcbaap_vaddr))) // 地址不是按照64byte对齐
    {
        kerror("dcbaap isn't 64 byte aligned.");
        goto failed_free_dyn;
    }
    // 写入dcbaap
    xhci_write_cap_reg64(cid, XHCI_OPS_DCBAAP, virt_2_phys(xhci_hc[cid].dcbaap_vaddr));
    xhci_hc[cid].cmd_ring_vaddr = xhci_create_ring(XHCI_CMND_RING_TRBS);
    ++xhci_ctrl_count;
    spin_unlock(&xhci_controller_init_lock);
    return;

failed_free_dyn:; // 释放动态申请的内存
    if (xhci_hc[cid].dcbaap_vaddr)
        kfree(xhci_hc[cid].dcbaap_vaddr);

failed:;
    // 取消地址映射
    mm_unmap(xhci_hc[cid].vbase, 65536);

    // 清空数组
    memset((void *)&xhci_hc[cid], 0, sizeof(struct xhci_host_controller_t));

failed_exceed_max:;
    kerror("Failed to initialize controller: bus=%d, dev=%d, func=%d", dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func);
    spin_unlock(&xhci_controller_init_lock);
}