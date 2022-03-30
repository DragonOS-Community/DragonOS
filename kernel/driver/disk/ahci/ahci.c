#include "ahci.h"
#include "../../../common/kprint.h"

struct pci_device_structure_header_t *ahci_devices[100];
uint32_t count_ahci_devices = 0;

/**
 * @brief 初始化ahci模块
 *
 */
void ahci_init()
{
    pci_get_device_structure(0x1, 0x6, ahci_devices, &count_ahci_devices);

    for(int i=0;i<count_ahci_devices;++i)
    {
        kdebug("[%d]  class_code=%d, sub_class=%d, progIF=%d", i, ahci_devices[i]->Class_code, ahci_devices[i]->SubClass, ahci_devices[i]->ProgIF);
    }
}