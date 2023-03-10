#include "msi.h"
#include "pci.h"
#include <common/errno.h>
#include <mm/mmio.h>

/**
 * @brief 生成msi消息
 *
 * @param msi_desc msi描述符
 * @return struct msi_msg_t* msi消息指针（在描述符内）
 */
extern struct msi_msg_t *msi_arch_get_msg(struct msi_desc_t *msi_desc);

/**
 * @brief 读取msix的capability list
 *
 * @param msi_desc msi描述符
 * @param cap_off capability list的offset
 * @return struct pci_msix_cap_t 对应的capability list
 */
static __always_inline struct pci_msix_cap_t __msi_read_msix_cap_list(struct msi_desc_t *msi_desc, uint32_t cap_off)
{
    struct pci_msix_cap_t cap_list = {0};
    uint32_t dw0;
    dw0 = pci_read_config(msi_desc->pci_dev->bus, msi_desc->pci_dev->device, msi_desc->pci_dev->func, cap_off);
    io_lfence();
    cap_list.cap_id = dw0 & 0xff;
    cap_list.next_off = (dw0 >> 8) & 0xff;
    cap_list.msg_ctrl = (dw0 >> 16) & 0xffff;

    cap_list.dword1 =
        pci_read_config(msi_desc->pci_dev->bus, msi_desc->pci_dev->device, msi_desc->pci_dev->func, cap_off + 0x4);
    cap_list.dword2 =
        pci_read_config(msi_desc->pci_dev->bus, msi_desc->pci_dev->device, msi_desc->pci_dev->func, cap_off + 0x8);
    return cap_list;
}

static __always_inline struct pci_msi_cap_t __msi_read_cap_list(struct msi_desc_t *msi_desc, uint32_t cap_off)
{
    struct pci_msi_cap_t cap_list = {0};
    uint32_t dw0;
    dw0 = pci_read_config(msi_desc->pci_dev->bus, msi_desc->pci_dev->device, msi_desc->pci_dev->func, cap_off);
    cap_list.cap_id = dw0 & 0xff;
    cap_list.next_off = (dw0 >> 8) & 0xff;
    cap_list.msg_ctrl = (dw0 >> 16) & 0xffff;

    cap_list.msg_addr_lo =
        pci_read_config(msi_desc->pci_dev->bus, msi_desc->pci_dev->device, msi_desc->pci_dev->func, cap_off + 0x4);
    uint16_t msg_data_off = 0xc;
    if (cap_list.msg_ctrl & (1 << 7)) // 64位
    {
        cap_list.msg_addr_hi =
            pci_read_config(msi_desc->pci_dev->bus, msi_desc->pci_dev->device, msi_desc->pci_dev->func, cap_off + 0x8);
    }
    else
    {
        cap_list.msg_addr_hi = 0;
        msg_data_off = 0x8;
    }

    cap_list.msg_data = pci_read_config(msi_desc->pci_dev->bus, msi_desc->pci_dev->device, msi_desc->pci_dev->func,
                                        cap_off + msg_data_off) &
                        0xffff;

    cap_list.mask =
        pci_read_config(msi_desc->pci_dev->bus, msi_desc->pci_dev->device, msi_desc->pci_dev->func, cap_off + 0x10);
    cap_list.pending =
        pci_read_config(msi_desc->pci_dev->bus, msi_desc->pci_dev->device, msi_desc->pci_dev->func, cap_off + 0x14);

    return cap_list;
}

/**
 * @brief 映射设备的msix表  //MSIX表不再单独映射(To do)
 *
 * @param pci_dev pci设备信息结构体
 * @param msix_cap msix capability list的结构体
 * @return int 错误码
 */
static __always_inline int __msix_map_table(struct pci_device_structure_header_t *pci_dev,
                                            struct pci_msix_cap_t *msix_cap)
{
    // 计算bar寄存器的offset
    uint32_t bar_off = 0x10 + 4 * (msix_cap->dword1 & 0x7);

    // msix table相对于bar寄存器中存储的地址的offset
    pci_dev->msix_offset = msix_cap->dword1 & (~0x7);
    pci_dev->msix_table_size = (msix_cap->msg_ctrl & 0x7ff) + 1;
    pci_dev->msix_mmio_size = pci_dev->msix_table_size * 16 + pci_dev->msix_offset;

    // 申请mmio空间
    mmio_create(pci_dev->msix_mmio_size, VM_IO | VM_DONTCOPY, &pci_dev->msix_mmio_vaddr, &pci_dev->msix_mmio_size);
    pci_dev->msix_mmio_vaddr &= (~0xf);
    uint32_t bar = pci_read_config(pci_dev->bus, pci_dev->device, pci_dev->func, bar_off);
    // kdebug("pci_dev->msix_mmio_vaddr=%#018lx, bar=%#010lx, table offset=%#010lx, table_size=%#010lx, mmio_size=%d",
    // pci_dev->msix_mmio_vaddr, bar, pci_dev->msix_offset, pci_dev->msix_table_size, pci_dev->msix_mmio_size);

    // 将msix table映射到页表
    mm_map(&initial_mm, pci_dev->msix_mmio_vaddr, pci_dev->msix_mmio_size, bar);
    return 0;
}

/**
 * @brief 将msi_desc中的数据填写到msix表的指定表项处
 *
 * @param pci_dev pci设备结构体
 * @param msi_desc msi描述符
 */
static __always_inline void __msix_set_entry(struct msi_desc_t *msi_desc)
{
    uint64_t *ptr =
        (uint64_t *)(msi_desc->pci_dev->msix_mmio_vaddr + msi_desc->pci_dev->msix_offset + msi_desc->msi_index * 16);
    *ptr = ((uint64_t)(msi_desc->msg.address_hi) << 32) | (msi_desc->msg.address_lo);
    io_mfence();
    ++ptr;
    io_mfence();
    *ptr = ((uint64_t)(msi_desc->msg.vector_control) << 32) | (msi_desc->msg.data);
    io_mfence();
}

/**
 * @brief 清空设备的msix table的指定表项
 *
 * @param pci_dev pci设备
 * @param msi_index 表项号
 */
static __always_inline void __msix_clear_entry(struct pci_device_structure_header_t *pci_dev, uint16_t msi_index)
{
    uint64_t *ptr = (uint64_t *)(pci_dev->msix_mmio_vaddr + pci_dev->msix_offset + msi_index * 16);
    *ptr = 0;
    ++ptr;
    *ptr = 0;
}

/**
 * @brief 启用 Message Signaled Interrupts
 *
 * @param header 设备header
 * @param vector 中断向量号
 * @param processor 要投递到的处理器
 * @param edge_trigger 是否边缘触发
 * @param assert 是否高电平触发
 *
 * @return 返回码
 */
int pci_enable_msi(struct msi_desc_t *msi_desc)
{
    struct pci_device_structure_header_t *ptr = msi_desc->pci_dev;
    uint32_t cap_ptr;
    uint32_t tmp;
    uint16_t message_control;
    uint64_t message_addr;

    // 先尝试获取msi-x，若不存在，则获取msi capability
    if (msi_desc->pci.msi_attribute.is_msix)
    {
        cap_ptr = pci_enumerate_capability_list(ptr, 0x11);
        if (((int32_t)cap_ptr) < 0)
        {
            cap_ptr = pci_enumerate_capability_list(ptr, 0x05);
            if (((int32_t)cap_ptr) < 0)
                return -ENOSYS;
            msi_desc->pci.msi_attribute.is_msix = 0;
        }
    }
    else
    {
        cap_ptr = pci_enumerate_capability_list(ptr, 0x05);
        if (((int32_t)cap_ptr) < 0)
            return -ENOSYS;
        msi_desc->pci.msi_attribute.is_msix = 0;
    }
    // 获取msi消息
    msi_arch_get_msg(msi_desc);

    if (msi_desc->pci.msi_attribute.is_msix) // MSI-X
    {
        kdebug("is msix");
        // 读取msix的信息
        struct pci_msix_cap_t cap = __msi_read_msix_cap_list(msi_desc, cap_ptr);
        // 映射msix table
        __msix_map_table(msi_desc->pci_dev, &cap);
        io_mfence();
        // 设置msix的中断
        __msix_set_entry(msi_desc);
        io_mfence();

        // todo: disable intx
        // 使能msi-x
        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值
        tmp |= (1U << 31);
        pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr, tmp);
    }
    else
    {
        kdebug("is msi");
        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值
        message_control = (tmp >> 16) & 0xffff;

        // 写入message address
        message_addr = ((((uint64_t)msi_desc->msg.address_hi) << 32) | msi_desc->msg.address_lo); // 获取message address
        pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr + 0x4, (uint32_t)(message_addr & 0xffffffff));

        if (message_control & (1 << 7)) // 64位
            pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr + 0x8,
                             (uint32_t)((message_addr >> 32) & 0xffffffff));

        // 写入message data

        tmp = msi_desc->msg.data;
        if (message_control & (1 << 7)) // 64位
            pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr + 0xc, tmp);
        else
            pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr + 0x8, tmp);

        // 使能msi
        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值
        tmp |= (1 << 16);
        pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr, tmp);
    }

    return 0;
}

/**
 * @brief 在已配置好msi寄存器的设备上，使能msi
 *
 * @param header 设备头部
 * @return int 返回码
 */
int pci_start_msi(void *header)
{
    struct pci_device_structure_header_t *ptr = (struct pci_device_structure_header_t *)header;
    uint32_t cap_ptr;
    uint32_t tmp;

    switch (ptr->HeaderType)
    {
    case 0x00: // general device
        if (!(ptr->Status & 0x10))
            return -ENOSYS;
        cap_ptr = ((struct pci_device_structure_general_device_t *)ptr)->Capabilities_Pointer;

        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值

        if (tmp & 0xff != 0x5)
            return -ENOSYS;

        // 使能msi
        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值
        tmp |= (1 << 16);
        pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr, tmp);

        break;

    case 0x01: // pci to pci bridge
        if (!(ptr->Status & 0x10))
            return -ENOSYS;
        cap_ptr = ((struct pci_device_structure_pci_to_pci_bridge_t *)ptr)->Capability_Pointer;

        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值

        if (tmp & 0xff != 0x5)
            return -ENOSYS;

        // 使能msi
        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值
        tmp |= (1 << 16);
        pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr, tmp);

        break;
    case 0x02: // pci to card bus bridge
        return -ENOSYS;
        break;

    default: // 不应该到达这里
        return -EINVAL;
        break;
    }

    return 0;
}
/**
 * @brief 禁用指定设备的msi
 *
 * @param header pci header
 * @return int
 */
int pci_disable_msi(void *header)
{
    struct pci_device_structure_header_t *ptr = (struct pci_device_structure_header_t *)header;
    uint32_t cap_ptr;
    uint32_t tmp;

    switch (ptr->HeaderType)
    {
    case 0x00: // general device
        if (!(ptr->Status & 0x10))
            return -ENOSYS;
        cap_ptr = ((struct pci_device_structure_general_device_t *)ptr)->Capabilities_Pointer;

        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值

        if (tmp & 0xff != 0x5)
            return -ENOSYS;

        // 禁用msi
        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值
        tmp &= (~(1 << 16));
        pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr, tmp);

        break;

    case 0x01: // pci to pci bridge
        if (!(ptr->Status & 0x10))
            return -ENOSYS;
        cap_ptr = ((struct pci_device_structure_pci_to_pci_bridge_t *)ptr)->Capability_Pointer;

        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值

        if (tmp & 0xff != 0x5)
            return -ENOSYS;

        // 禁用msi
        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值
        tmp &= (~(1 << 16));
        pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr, tmp);

        break;
    case 0x02: // pci to card bus bridge
        return -ENOSYS;
        break;

    default: // 不应该到达这里
        return -EINVAL;
        break;
    }

    return 0;
}