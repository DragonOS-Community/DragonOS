#include "ahci.h"
#include <common/kprint.h>
#include <mm/slab.h>
#include <common/string.h>
#include <common/block.h>
#include <debug/bug.h>
#include <common/kthread.h>

/// @brief 保留了对 pci设备获取 和 mm内存映射 的依赖
void ahci_cpp_init(uint32_t *count_ahci_devices, struct pci_device_structure_header_t *ahci_devs[MAX_AHCI_DEVICES], struct pci_device_structure_general_device_t *gen_devs[MAX_AHCI_DEVICES])
{
    kinfo("Initializing AHCI...");

    pci_get_device_structure(0x1, 0x6, ahci_devs, count_ahci_devices);

    if (*count_ahci_devices == 0)
    {
        kwarn("There is no AHCI device found on this computer!");
        return;
    }

    for (int i = 0; i < *count_ahci_devices; i++)
    {
        gen_devs[i] = ((struct pci_device_structure_general_device_t *)(ahci_devs[i]));
    }

    // 映射ABAR
    uint32_t bar5 = gen_devs[0]->BAR5;
    mm_map_phys_addr(AHCI_MAPPING_BASE, (ul)(bar5)&PAGE_2M_MASK, PAGE_2M_SIZE, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD, false);

    kinfo("ABAR mapped!");
}
