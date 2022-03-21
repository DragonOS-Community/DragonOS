#include "pci.h"
#include "../../common/kprint.h"
#include "../../mm/slab.h"

static uint count_device_list = 0;

/**
 * @brief 将设备信息结构体加到链表里面
 *
 */
#define ADD_DEVICE_STRUCT_TO_LIST(ret)                               \
    do                                                               \
    {                                                                \
        if (count_device_list > 0)                                   \
        {                                                            \
            ++count_device_list;                                     \
            list_add(pci_device_structure_list, &ret->header->list); \
        }                                                            \
        else                                                         \
        {                                                            \
            ++count_device_list;                                     \
            list_init(&ret->header->list);                           \
            pci_device_structure_list = &ret->header->list;          \
        }                                                            \
    } while (0)

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
    header->Interrupt_Line = tmp32 & 0xff;
    header->Interrupt_PIN = (tmp32 >> 8) & 0xff;
    header->Bridge_Control = (tmp32 >> 16) & 0xffff;

    tmp32 = pci_read_config(bus, slot, func, 0x40);
    header->Subsystem_Device_ID = tmp32 & 0xffff;
    header->Subsystem_Vendor_ID = (tmp32 >> 16) & 0xffff;

    header->PC_Card_legacy_mode_base_address_16_bit = pci_read_config(bus, slot, func, 0x44);
}

/**
 * @brief 读取pci设备标头
 *
 * @param type 标头类型
 * @param bus 总线号
 * @param slot 插槽号
 * @param func 功能号
 * @param add_to_list 添加到链表
 * @return 返回的header
 */
void *pci_read_header(int *type, uchar bus, uchar slot, uchar func, bool add_to_list)
{
    struct pci_device_structure_header_t *common_header = (struct pci_device_structure_header_t *)kmalloc(sizeof(struct pci_device_structure_header_t), 0);

    uint32_t tmp32;
    // 先读取公共header
    tmp32 = pci_read_config(bus, slot, func, 0x0);
    common_header->Vendor_ID = tmp32 & 0xffff;
    common_header->Device_ID = (tmp32 >> 16) & 0xffff;

    tmp32 = pci_read_config(bus, slot, func, 0x4);
    common_header->Command = tmp32 & 0xffff;
    common_header->Status = (tmp32 >> 16) & 0xffff;

    tmp32 = pci_read_config(bus, slot, func, 0x8);
    common_header->RevisionID = tmp32 & 0xff;
    common_header->ProgIF = (tmp32 >> 8) & 0xff;
    common_header->SubClass = (tmp32 >> 16) & 0xff;
    common_header->Class_code = (tmp32 >> 24) & 0xff;

    tmp32 = pci_read_config(bus, slot, func, 0xc);
    common_header->CacheLineSize = tmp32 & 0xff;
    common_header->LatencyTimer = (tmp32 >> 8) & 0xff;
    common_header->HeaderType = (tmp32 >> 16) & 0xff;
    common_header->BIST = (tmp32 >> 24) & 0xff;

    // 根据公共头部，判断该结构所属的类型
    switch (common_header->Vendor_ID)
    {
    case 0xFFFF: // 设备不可用
        *type = E_DEVICE_INVALID;
        kfree(common_header);
        return NULL;
        break;
    case 0x0: // general device
        struct pci_device_structure_general_device_t *ret = (struct pci_device_structure_general_device_t *)kmalloc(sizeof(struct pci_device_structure_general_device_t), 0);

        ret->header = common_header;
        pci_read_general_device_header(ret, bus, slot, func);
        if (add_to_list)
            ADD_DEVICE_STRUCT_TO_LIST(ret);

        *type = 0x0;
        return ret;
        break;
    case 0x1:
        struct pci_device_structure_pci_to_pci_bridge_t *ret = (struct pci_device_structure_pci_to_pci_bridge_t *)kmalloc(sizeof(struct pci_device_structure_pci_to_pci_bridge_t), 0);
        ret->header = common_header;
        pci_read_pci_to_pci_bridge_header(ret, bus, slot, func);
        if (add_to_list)
            ADD_DEVICE_STRUCT_TO_LIST(ret);

        *type = 0x1;
        return ret;
        break;
    case 0x2:
        struct pci_device_structure_pci_to_cardbus_bridge_t *ret = (struct pci_device_structure_pci_to_cardbus_bridge_t *)kmalloc(sizeof(struct pci_device_structure_pci_to_cardbus_bridge_t), 0);
        ret->header = common_header;
        pci_read_pci_to_cardbus_bridge_header(ret, bus, slot, func);
        if (add_to_list)
            ADD_DEVICE_STRUCT_TO_LIST(ret);

        *type = 0x2;
        return ret;
        break;
    default: // 错误的头类型 这里不应该被执行
        kBUG("PCI->pci_read_header(): Invalid header type.");
        *type = E_WRONG_HEADER_TYPE;
        kfree(common_header);
        return NULL;
        break;
    }
}
static void pci_checkFunction(uint8_t bus, uint8_t device, uint8_t function)
{
    int header_type;
    struct pci_device_structure_header_t *header;
    void *raw_header = pci_read_header(&header_type, 0, 0, 0, true);

    if (header_type == E_WRONG_HEADER_TYPE)
    {
        kBUG("pci_checkFunction(): wrong header type!");
        // 此处内存已经在read header函数里面释放，不用重复释放
        return;
    }
    header = ((struct pci_device_structure_general_device_t *)raw_header)->header;

    if ((header->Class_code == 0x6) && (header->SubClass == 0x4))
    {
        uint8_t SecondaryBus = ((struct pci_device_structure_pci_to_pci_bridge_t *)raw_header)->Secondary_Bus_Number;
        pci_checkBus(SecondaryBus);
    }

    kfree(header);
    kfree(raw_header);
}

static void pci_checkDevice(uint8_t bus, uint8_t device)
{
    int header_type;
    void *raw_header;
    struct pci_device_structure_header_t *header;
    raw_header = pci_read_header(&header_type, bus, device, 0, false);
    if (header_type == E_WRONG_HEADER_TYPE)
    {
        // 此处内存已经在read header函数里面释放，不用重复释放
        kBUG("pci_checkDevice(): wrong header type!");
        return;
    }
    header = ((struct pci_device_structure_general_device_t *)raw_header)->header;
    uint16_t vendorID = header->Vendor_ID;

    if (vendorID == 0xffff) // 设备不存在
    {
        kfree(header);
        kfree(raw_header);
        return;
    }
    pci_checkFunction(bus, device, 0);

    header_type = header->HeaderType;
    if ((header_type & 0x80) != 0)
    {
        // 这是一个多function的设备，因此查询剩余的function
        for (uint8_t func = 1; func < 8; ++func)
        {
            struct pci_device_structure_header_t *tmp_header;
            void *tmp_raw_header;
            tmp_raw_header = pci_read_header(&header_type, bus, device, func, false);
            tmp_header = ((struct pci_device_structure_general_device_t *)tmp_raw_header)->header;

            pci_checkFunction(bus, device, func);

            // 释放内存
            kfree(tmp_header);
            kfree(tmp_raw_header);
        }
    }

    kfree(header);
    kfree(raw_header);
}

static void pci_checkBus(uint8_t bus)
{
    for (uint8_t device = 0; device < 32; ++device)
        pci_checkDevice(bus, device);
}

/**
 * @brief 扫描所有pci总线上的所有设备
 *
 */
void pci_checkAllBuses()
{
    int header_type;
    void *raw_header;
    struct pci_device_structure_header_t *header;
    raw_header = pci_read_header(&header_type, 0, 0, 0, false);

    if (header_type == E_WRONG_HEADER_TYPE)
    {
        kBUG("pci_checkAllBuses(): wrong header type!");
        // 此处内存已经在read header函数里面释放，不用重复释放
        return;
    }
    header = ((struct pci_device_structure_general_device_t *)raw_header)->header;

    header_type = header->HeaderType;

    if ((header_type & 0x80) == 0) // Single pci host controller
    {
        pci_checkBus(0);
    }
    else
    {
        // Multiple PCI host controller
        // 那么总线0，设备0，功能1则是总线1的pci主机控制器，以此类推
        void *tmp_raw_header;
        struct pci_device_structure_header_t *tmp_header;
        for (uint8_t func = 0; func < 8; ++func)
        {
            tmp_raw_header = pci_read_header(&header_type, 0, 0, func, false);
            tmp_header = ((struct pci_device_structure_general_device_t *)tmp_raw_header)->header;
            if (header->Vendor_ID != 0xffff)    // @todo 这里的判断条件可能有点问题
            {
                kfree(tmp_header);
                kfree(tmp_raw_header);
                break;
            }

            pci_checkBus(func);

            kfree(tmp_header);
            kfree(tmp_raw_header);
        }
    }
    kfree(header);
    kfree(raw_header);
}

void pci_init()
{
}