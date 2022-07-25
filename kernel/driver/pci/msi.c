#include "msi.h"
#include "pci.h"

/**
 * @brief 生成架构相关的msi的message address
 *
 */
#define pci_get_arch_msi_message_address(processor) ((uint64_t)(0xfee00000UL | (processor << 12)))

/**
 * @brief 生成架构相关的message data
 *
 */
#define pci_get_arch_msi_message_data(vector, processor, edge_trigger, assert) ((uint32_t)((vector & 0xff) | (edge_trigger == 1 ? 0 : (1 << 15)) | ((assert == 0) ? 0 : (1 << 14))))


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
int pci_enable_msi(void *header, uint8_t vector, uint32_t processor, uint8_t edge_trigger, uint8_t assert)
{
    struct pci_device_structure_header_t *ptr = (struct pci_device_structure_header_t *)header;
    uint32_t cap_ptr;
    uint32_t tmp;
    uint16_t message_control;
    uint64_t message_addr;
    switch (ptr->HeaderType)
    {
    case 0x00: // general device
        if (!(ptr->Status & 0x10))
            return E_NOT_SUPPORT_MSI;    
        
        cap_ptr = ((struct pci_device_structure_general_device_t *)ptr)->Capabilities_Pointer;

        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值
        message_control = (tmp >> 16) & 0xffff;

        if (tmp & 0xff != 0x5)
            return E_NOT_SUPPORT_MSI;

        // 写入message address
        message_addr = pci_get_arch_msi_message_address(processor); // 获取message address
        pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr + 0x4, (uint32_t)(message_addr & 0xffffffff));

        if (message_control & (1 << 7)) // 64位
            pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr + 0x8, (uint32_t)((message_addr >> 32) & 0xffffffff));

        // 写入message data
        tmp = pci_get_arch_msi_message_data(vector, processor, edge_trigger, assert);
        if (message_control & (1 << 7)) // 64位
            pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr + 0xc, tmp);
        else
            pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr + 0x8, tmp);

        // 使能msi
        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值
        tmp |= (1 << 16);
        pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr, tmp);

        break;

    case 0x01: // pci to pci bridge
        if (!(ptr->Status & 0x10))
            return E_NOT_SUPPORT_MSI;
        cap_ptr = ((struct pci_device_structure_pci_to_pci_bridge_t *)ptr)->Capability_Pointer;

        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值

        message_control = (tmp >> 16) & 0xffff;

        if (tmp & 0xff != 0x5)
            return E_NOT_SUPPORT_MSI;

        // 写入message address
        message_addr = pci_get_arch_msi_message_address(processor); // 获取message address
        pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr + 0x4, (uint32_t)(message_addr & 0xffffffff));

        if (message_control & (1 << 7)) // 64位
            pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr + 0x8, (uint32_t)((message_addr >> 32) & 0xffffffff));

        // 写入message data
        tmp = pci_get_arch_msi_message_data(vector, processor, edge_trigger, assert);
        if (message_control & (1 << 7)) // 64位
            pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr + 0xc, tmp);
        else
            pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr + 0x8, tmp);

        // 使能msi
        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值
        tmp |= (1 << 16);
        pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr, tmp);

        break;
    case 0x02: // pci to card bus bridge
        return E_NOT_SUPPORT_MSI;
        break;

    default: // 不应该到达这里
        return E_WRONG_HEADER_TYPE;
        break;
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
            return E_NOT_SUPPORT_MSI;
        cap_ptr = ((struct pci_device_structure_general_device_t *)ptr)->Capabilities_Pointer;

        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值

        if (tmp & 0xff != 0x5)
            return E_NOT_SUPPORT_MSI;

        // 使能msi
        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值
        tmp |= (1 << 16);
        pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr, tmp);

        break;

    case 0x01: // pci to pci bridge
        if (!(ptr->Status & 0x10))
            return E_NOT_SUPPORT_MSI;
        cap_ptr = ((struct pci_device_structure_pci_to_pci_bridge_t *)ptr)->Capability_Pointer;

        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值

        if (tmp & 0xff != 0x5)
            return E_NOT_SUPPORT_MSI;

        //使能msi
        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值
        tmp |= (1 << 16);
        pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr, tmp);

        break;
    case 0x02: // pci to card bus bridge
        return E_NOT_SUPPORT_MSI;
        break;

    default: // 不应该到达这里
        return E_WRONG_HEADER_TYPE;
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
            return E_NOT_SUPPORT_MSI;
        cap_ptr = ((struct pci_device_structure_general_device_t *)ptr)->Capabilities_Pointer;

        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值

        if (tmp & 0xff != 0x5)
            return E_NOT_SUPPORT_MSI;

        // 禁用msi
        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值
        tmp &= (~(1 << 16));
        pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr, tmp);

        break;

    case 0x01: // pci to pci bridge
        if (!(ptr->Status & 0x10))
            return E_NOT_SUPPORT_MSI;
        cap_ptr = ((struct pci_device_structure_pci_to_pci_bridge_t *)ptr)->Capability_Pointer;

        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值

        if (tmp & 0xff != 0x5)
            return E_NOT_SUPPORT_MSI;

        //禁用msi
        tmp = pci_read_config(ptr->bus, ptr->device, ptr->func, cap_ptr); // 读取cap+0x0处的值
        tmp &= (~(1 << 16));
        pci_write_config(ptr->bus, ptr->device, ptr->func, cap_ptr, tmp);

        break;
    case 0x02: // pci to card bus bridge
        return E_NOT_SUPPORT_MSI;
        break;

    default: // 不应该到达这里
        return E_WRONG_HEADER_TYPE;
        break;
    }

    return 0;
}