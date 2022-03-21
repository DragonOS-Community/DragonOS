#include "pci.h"
#include "../../common/kprint.h"

/**
 * @brief 从pci配置空间读取信息
 *
 * @param bus 总线号
 * @param slot 设备号
 * @param func 功能号
 * @param offset 寄存器偏移量
 * @return uint 寄存器值
 */
uint pci_read_config(uchar bus, uchar slot, uchar func, uchar offset)
{
    uint lbus = (uint)bus;
    uint lslot = (uint)slot;
    uint lfunc = ((uint)func) & 7;

    // 构造pci配置空间地址
    uint address = (uint)((lbus << 16) | (lslot << 11) | (lfunc << 8) | (offset & 0xfc) | ((uint)0x80000000));
    io_out32(PORT_PCI_CONFIG_ADDRESS, address);
    // 读取返回的数据
    return (uint)(io_in32(PORT_PCI_CONFIG_DATA));
}

/**
 * @brief 读取type为0x0的pci设备的header
 * 本函数只应被 pci_read_header()调用
 * @param header 返回的header
 * @param bus 总线号
 * @param slot 插槽号
 * @param func 功能号
 */
static void pci_read_general_device_header(struct pci_device_structure_general_device_t *header, uchar bus, uchar slot, uchar func)
{
    uint32_t tmp32;
    header->BAR0 = pci_read_config(bus, slot, func, 0x10);
    header->BAR1 = pci_read_config(bus, slot, func, 0x14);
    header->BAR2 = pci_read_config(bus, slot, func, 0x18);
    header->BAR3 = pci_read_config(bus, slot, func, 0x1c);
    header->BAR4 = pci_read_config(bus, slot, func, 0x20);
    header->BAR5 = pci_read_config(bus, slot, func, 0x24);
    header->Cardbus_CIS_Pointer = pci_read_config(bus, slot, func, 0x28);

    tmp32 = pci_read_config(bus, slot, func, 0x2c);
    header->Subsystem_Vendor_ID = tmp32 & 0xffff;
    header->Subsystem_ID = (tmp32 >> 16) & 0xffff;

    header->Expansion_ROM_base_address = pci_read_config(bus, slot, func, 0x30);

    tmp32 = pci_read_config(bus, slot, func, 0x34);
    header->Capabilities_Pointer = tmp32 & 0xff;
    header->reserved0 = (tmp32 >> 8) & 0xff;
    header->reserved1 = (tmp32 >> 16) & 0xffff;

    header->reserved2 = pci_read_config(bus, slot, func, 0x38);

    tmp32 = pci_read_config(bus, slot, func, 0x3c);
    header->Interrupt_Line = tmp32 & 0xff;
    header->Interrupt_PIN = (tmp32 >> 8) & 0xff;
    header->Min_Grant = (tmp32 >> 16) & 0xff;
    header->Max_Latency = (tmp32 >> 24) & 0xff;
}

/**
 * @brief 读取type为0x1的pci_to_pci_bridge的header
 * 本函数只应被 pci_read_header()调用
 * @param header 返回的header
 * @param bus 总线号
 * @param slot 插槽号
 * @param func 功能号
 */
static void pci_read_pci_to_pci_bridge_header(struct pci_device_structure_pci_to_pci_bridge_t *header, uchar bus, uchar slot, uchar func)
{
    uint32_t tmp32;
    header->BAR0 = pci_read_config(bus, slot, func, 0x10);
    header->BAR1 = pci_read_config(bus, slot, func, 0x14);

    tmp32 = pci_read_config(bus, slot, func, 0x18);

    header->Primary_Bus_Number = tmp32 & 0xff;
    header->Secondary_Bus_Number = (tmp32 >> 8) & 0xff;
    header->Subordinate_Bus_Number = (tmp32 >> 16) & 0xff;
    header->Secondary_Latency_Timer = (tmp32 >> 24) & 0xff;

    tmp32 = pci_read_config(bus, slot, func, 0x1c);
    header->io_base = tmp32 & 0xff;
    header->io_limit = (tmp32 >> 8) & 0xff;
    header->Secondary_Status = (tmp32 >> 16) & 0xffff;

    tmp32 = pci_read_config(bus, slot, func, 0x20);
    header->Memory_Base = tmp32 & 0xffff;
    header->Memory_Limit = (tmp32 >> 16) & 0xffff;

    tmp32 = pci_read_config(bus, slot, func, 0x24);
    header->Prefetchable_Memory_Base = tmp32 & 0xffff;
    header->Prefetchable_Memory_Limit = (tmp32 >> 16) & 0xffff;

    header->Prefetchable_Base_Upper_32_Bits = pci_read_config(bus, slot, func, 0x28);
    header->Prefetchable_Limit_Upper_32_Bits = pci_read_config(bus, slot, func, 0x2c);

    tmp32 = pci_read_config(bus, slot, func, 0x30);
    header->io_Base_Upper_16_Bits = tmp32 & 0xffff;
    header->io_Limit_Upper_16_Bits = (tmp32 >> 16) & 0xffff;

    tmp32 = pci_read_config(bus, slot, func, 0x34);
    header->Capability_Pointer = tmp32 & 0xff;
    header->reserved0 = (tmp32 >> 8) & 0xff;
    header->reserved1 = (tmp32 >> 16) & 0xffff;

    header->Expansion_ROM_base_address = pci_read_config(bus, slot, func, 0x38);

    tmp32 = pci_read_config(bus, slot, func, 0x3c);
    header->Interrupt_Line = tmp32 & 0xff;
    header->Interrupt_PIN = (tmp32 >> 8) & 0xff;
    header->Bridge_Control = (tmp32 >> 16) & 0xffff;
}

/**
 * @brief 读取type为0x2的pci_to_cardbus_bridge的header
 * 本函数只应被 pci_read_header()调用
 * @param header 返回的header
 * @param bus 总线号
 * @param slot 插槽号
 * @param func 功能号
 */
static void pci_read_pci_to_cardbus_bridge_header(struct pci_device_structure_pci_to_cardbus_bridge_t *header, uchar bus, uchar slot, uchar func)
{
    uint32_t tmp32;

    header->CardBus_Socket_ExCa_base_address = pci_read_config(bus, slot, func, 0x10);

    tmp32 = pci_read_config(bus, slot, func, 0x14);
    header->Offset_of_capabilities_list = tmp32 & 0xff;
    header->Reserved = (tmp32 >> 8) & 0xff;
    header->Secondary_status = (tmp32 >> 16) & 0xff;

    tmp32 = pci_read_config(bus, slot, func, 0x18);
    header->PCI_bus_number = tmp32 & 0xff;
    header->CardBus_bus_number = (tmp32 >> 8) & 0xff;
    header->Subordinate_bus_number = (tmp32 >> 16) & 0xff;
    header->CardBus_latency_timer = (tmp32 >> 24) & 0xff;

    header->Memory_Base_Address0 = pci_read_config(bus, slot, func, 0x1c);
    header->Memory_Limit0 = pci_read_config(bus, slot, func, 0x20);
    header->Memory_Base_Address1 = pci_read_config(bus, slot, func, 0x24);
    header->Memory_Limit1 = pci_read_config(bus, slot, func, 0x28);

    header->IO_Base_Address0 = pci_read_config(bus, slot, func, 0x2c);
    header->IO_Limit0 = pci_read_config(bus, slot, func, 0x30);
    header->IO_Base_Address1 = pci_read_config(bus, slot, func, 0x34);
    header->IO_Limit1 = pci_read_config(bus, slot, func, 0x38);

    tmp32 = pci_read_config(bus, slot, func, 0x3c);
    header->Interrupt_Line = tmp32&0xff;
    header->Interrupt_PIN = (tmp32>>8)&0xff;
    header->Bridge_Control = (tmp32>>16)&0xffff;
    
    tmp32 = pci_read_config(bus, slot, func, 0x40);
    header->Subsystem_Device_ID = tmp32&0xffff;
    header->Subsystem_Vendor_ID = (tmp32>>16)&0xffff;

    header->PC_Card_legacy_mode_base_address_16_bit = pci_read_config(bus, slot, func, 0x44);
}

/**
 * @brief 读取pci设备标头
 *
 * @param type 标头类型
 * @param bus 总线号
 * @param slot 插槽号
 * @param func 功能号
 * @return 返回的header
 */
void *pci_read_header(int *type, uchar bus, uchar slot, uchar func)
{
    struct pci_device_structure_header_t common_header;
    uint32_t tmp32;
    // 先读取公共header
    tmp32 = pci_read_config(bus, slot, func, 0x0);
    common_header.Vendor_ID = tmp32 & 0xffff;
    common_header.Device_ID = (tmp32 >> 16) & 0xffff;

    tmp32 = pci_read_config(bus, slot, func, 0x4);
    common_header.Command = tmp32 & 0xffff;
    common_header.Status = (tmp32 >> 16) & 0xffff;

    tmp32 = pci_read_config(bus, slot, func, 0x8);
    common_header.RevisionID = tmp32 & 0xff;
    common_header.ProgIF = (tmp32 >> 8) & 0xff;
    common_header.SubClass = (tmp32 >> 16) & 0xff;
    common_header.Class_code = (tmp32 >> 24) & 0xff;

    tmp32 = pci_read_config(bus, slot, func, 0xc);
    common_header.CacheLineSize = tmp32 & 0xff;
    common_header.LatencyTimer = (tmp32 >> 8) & 0xff;
    common_header.HeaderType = (tmp32 >> 16) & 0xff;
    common_header.BIST = (tmp32 >> 24) & 0xff;

    // 根据公共头部，判断该结构所属的类型
    switch (common_header.Vendor_ID)
    {
    case 0xFFFF: // 设备不可用
        *type = E_DEVICE_INVALID;
        return NULL;
        break;
    case 0x0: // general device
        struct pci_device_structure_general_device_t ret;
        ret.header = common_header;
        pci_read_general_device_header(&ret, bus, slot, func);
        *type = 0x0;
        return &ret;
        break;
    case 0x1:
        struct pci_device_structure_pci_to_pci_bridge_t ret;
        ret.header = common_header;
        *type = 0x1;
        pci_read_pci_to_pci_bridge_header(&ret, bus, slot, func);
        return &ret;
        break;
    case 0x2:
        struct pci_device_structure_pci_to_cardbus_bridge_t ret;
        ret.header = common_header;
        *type = 0x2;
        pci_read_pci_to_cardbus_bridge_header(&ret, bus, slot, func);
        return &ret;
        break;
    default: // 错误的头类型 这里不应该被执行
        kBUG("PCI->pci_read_header(): Invalid header type.");
        *type = E_WRONG_HEADER_TYPE;
        return NULL;
        break;
    }
}

void pci_init()
{
}