#include "xhci.h"
#include <common/kprint.h>
#include <debug/bug.h>
#include <process/spinlock.h>
#include <mm/mm.h>
#include <debug/traceback/traceback.h>
#include <common/time.h>

spinlock_t xhci_controller_init_lock; // xhci控制器初始化锁(在usb_init中被初始化)

static int xhci_ctrl_count = 0; // xhci控制器计数

static struct xhci_host_controller_t xhci_hc[MAX_XHCI_HOST_CONTROLLERS] = {0};

#define xhci_read_cap_reg8(id, offset) (*(uint8_t *)(xhci_hc[id].vbase + offset))
#define xhci_write_cap_reg8(id, offset, value) (*(uint8_t *)(xhci_hc[id].vbase + offset) = (uint8_t)value)
#define xhci_read_cap_reg32(id, offset) (*(uint32_t *)(xhci_hc[id].vbase + offset))
#define xhci_write_cap_reg32(id, offset, value) (*(uint32_t *)(xhci_hc[id].vbase + offset) = (uint32_t)value)
#define xhci_read_cap_reg64(id, offset) (*(uint64_t *)(xhci_hc[id].vbase + offset))
#define xhci_write_cap_reg64(id, offset, value) (*(uint64_t *)(xhci_hc[id].vbase + offset) = (uint64_t)value)

#define xhci_read_op_reg8(id, offset) (*(uint8_t *)(xhci_hc[id].vbase_op + offset))
#define xhci_write_op_reg8(id, offset, value) (*(uint8_t *)(xhci_hc[id].vbase_op + offset) = (uint8_t)value)
#define xhci_read_op_reg32(id, offset) (*(uint32_t *)(xhci_hc[id].vbase_op + offset))
#define xhci_write_op_reg32(id, offset, value) (*(uint32_t *)(xhci_hc[id].vbase_op + offset) = (uint32_t)value)
#define xhci_read_op_reg64(id, offset) (*(uint64_t *)(xhci_hc[id].vbase_op + offset))
#define xhci_write_op_reg64(id, offset, value) (*(uint64_t *)(xhci_hc[id].vbase_op + offset) = (uint64_t)value)

/**
 * @brief 停止xhci主机控制器
 *
 * @param id 主机控制器id
 * @return int
 */
static int xhci_hc_stop(int id)
{
    // todo: 停止usb控制器
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
        if (retval)
            return retval;
    }
    int timeout = 500; // wait 500ms
    // reset
    xhci_write_cap_reg32(id, XHCI_OPS_USBCMD, (1 << 1));
    usleep(1000);
    while (xhci_read_op_reg32(id, XHCI_OPS_USBCMD) & (1 << 1))
    {
        usleep(1000);
        if (--timeout == 0)
            return -ETIMEDOUT;
    }
    kdebug("reset done!, timeout=%d", timeout);
    return retval;
}

/**
 * @brief 初始化xhci控制器
 *
 * @param header 指定控制器的pci device头部
 */
void xhci_init(struct pci_device_structure_general_device_t *dev_hdr)
{
    spin_lock(&xhci_controller_init_lock);
    kinfo("Initializing xhci host controller: bus=%#02x, device=%#02x, func=%#02x, VendorID=%#04x, irq_line=%d, irq_pin=%d", dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, dev_hdr->header.Vendor_ID, dev_hdr->Interrupt_Line, dev_hdr->Interrupt_PIN);

    xhci_hc[xhci_ctrl_count].controller_id = xhci_ctrl_count;
    xhci_hc[xhci_ctrl_count].pci_dev_hdr = dev_hdr;
    pci_write_config(dev_hdr->header.bus, dev_hdr->header.device, dev_hdr->header.func, 0x4, 0x0006); // mem I/O access enable and bus master enable

    // 为当前控制器映射寄存器地址空间
    xhci_hc[xhci_ctrl_count].vbase = SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + XHCI_MAPPING_OFFSET + PAGE_2M_SIZE * xhci_hc[xhci_ctrl_count].controller_id;
    kdebug("dev_hdr->BAR0 & (~0xf)=%#018lx", dev_hdr->BAR0 & (~0xf));
    mm_map_phys_addr(xhci_hc[xhci_ctrl_count].vbase, dev_hdr->BAR0 & (~0xf), 65536, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD, true);

    // 读取xhci控制寄存器
    uint16_t iversion = *(uint16_t *)(xhci_hc[xhci_ctrl_count].vbase + XHCI_CAPS_HCIVERSION);

    // 计算operational registers的地址
    xhci_hc[xhci_ctrl_count].vbase_op = xhci_hc[xhci_ctrl_count].vbase + xhci_read_cap_reg8(xhci_ctrl_count, XHCI_CAPS_CAPLENGTH);

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
    
    xhci_hc_reset(xhci_ctrl_count);
    ++xhci_ctrl_count;
    spin_unlock(&xhci_controller_init_lock);
    return;
failed:;
    // 取消地址映射
    mm_unmap(xhci_hc[xhci_ctrl_count].vbase, 65536);

    // 清空数组
    memset((void *)&xhci_hc[xhci_ctrl_count], 0, sizeof(struct xhci_host_controller_t));

    spin_unlock(&xhci_controller_init_lock);
}