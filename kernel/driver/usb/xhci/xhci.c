#include "xhci.h"
#include <common/kprint.h>
#include <debug/bug.h>
#include <process/spinlock.h>
#include <mm/mm.h>
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

#define FAIL_ON(value, to)        \
    do                            \
    {                             \
        if (unlikely(value != 0)) \
            goto to;              \
    } while (0)

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
 * @brief 配对xhci主机控制器的usb2、usb3端口
 *
 * @param id 主机控制器id
 * @return int 返回码
 */
static int xhci_hc_pair_ports(int id)
{
    struct xhci_caps_HCCPARAMS1_reg_t hcc1;
    struct xhci_caps_HCCPARAMS2_reg_t hcc2;

    struct xhci_caps_HCSPARAMS1_reg_t hcs1;
    struct xhci_caps_HCSPARAMS2_reg_t hcs2;
    memcpy(&hcc1, xhci_get_ptr_cap_reg32(id, XHCI_CAPS_HCCPARAMS1), sizeof(struct xhci_caps_HCCPARAMS1_reg_t));
    memcpy(&hcc2, xhci_get_ptr_cap_reg32(id, XHCI_CAPS_HCCPARAMS2), sizeof(struct xhci_caps_HCCPARAMS1_reg_t));
    memcpy(&hcs1, xhci_get_ptr_cap_reg32(id, XHCI_CAPS_HCSPARAMS1), sizeof(struct xhci_caps_HCCPARAMS1_reg_t));
    memcpy(&hcs2, xhci_get_ptr_cap_reg32(id, XHCI_CAPS_HCSPARAMS2), sizeof(struct xhci_caps_HCCPARAMS1_reg_t));

    // 从hcs1获取端口数量
    xhci_hc[id].port_num = hcs1.max_ports;
    kinfo("Found %d ports on xhci root hub.", hcs1.max_ports);

    // 找到所有的端口并标记其端口信息

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
    kdebug("dev_hdr->BAR0 & (~0xf)=%#018lx", dev_hdr->BAR0 & (~0xf));
    mm_map_phys_addr(xhci_hc[cid].vbase, dev_hdr->BAR0 & (~0xf), 65536, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD, true);

    // 读取xhci控制寄存器
    uint16_t iversion = *(uint16_t *)(xhci_hc[cid].vbase + XHCI_CAPS_HCIVERSION);
    uint32_t hcc1 = xhci_read_cap_reg32(cid, XHCI_CAPS_HCCPARAMS1);

    // 计算operational registers的地址
    xhci_hc[cid].vbase_op = xhci_hc[cid].vbase + xhci_read_cap_reg8(cid, XHCI_CAPS_CAPLENGTH);

    xhci_hc[cid].db_offset = xhci_read_cap_reg32(cid, XHCI_CAPS_DBOFF) & (~0x3);    // bits [1:0] reserved
    xhci_hc[cid].rts_offset = xhci_read_cap_reg32(cid, XHCI_CAPS_RTSOFF) & (~0x1f); // bits [4:0] reserved.

    xhci_hc[cid].ext_caps_off = ((hcc1 & 0xffff0000) >> 16) * 4;
    xhci_hc[cid].context_size = (hcc1 & (1 << 2)) ? 64 : 32;

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
    FAIL_ON(xhci_hc_stop_legacy(cid), failed);
    FAIL_ON(xhci_hc_pair_ports(cid), failed);

    ++xhci_ctrl_count;
    spin_unlock(&xhci_controller_init_lock);
    return;
failed:;
    // 取消地址映射
    mm_unmap(xhci_hc[cid].vbase, 65536);

    // 清空数组
    memset((void *)&xhci_hc[cid], 0, sizeof(struct xhci_host_controller_t));
failed_exceed_max:;
    kerror("Failed to initialize controller: bus=%d, dev=%d, func=%d", dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func);
    spin_unlock(&xhci_controller_init_lock);
}