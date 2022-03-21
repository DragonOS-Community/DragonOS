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
    uint address = (uint)((lbus << 16) | (lslot << 11) | (lfunc << 8)|(offset&0xfc)|((uint)0x80000000));
    io_out32(PORT_PCI_CONFIG_ADDRESS, address);
    // 读取返回的数据
    return (uint)(io_in32(PORT_PCI_CONFIG_DATA));
}

void pci_init()
{
    
}