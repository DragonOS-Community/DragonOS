#include "virtio.h"
#include <common/kprint.h>
#include <common/errno.h>
#include <driver/pci/pci.h>

#define MAX_NET_NUM 8 // pci总线上的net设备的最大数量

// 在pci总线上寻找到net设备控制器的header
static struct pci_device_structure_header_t *net_pdevs[MAX_NET_NUM];
static int net_pdevs_count = 0;
static struct pci_device_structure_header_t *virtio_net_pdev;
static int virtio_net_pdev_count = 0;
static uint8_t NETWORK_CLASS = 0x2;
static uint8_t ETHERNET_SUBCLASS = 0x0;

/** 
 * @brief 获取virtio-net MMIO映射的虚拟地址
 * @param virt_addr 外部传入的虚拟地址指针
 * @return 获取成功，返回0,失败，返回错误码
 */
uint8_t get_virtio_net_device(uint8_t * bus, uint8_t *device,uint8_t * function)
{
    // 获取所有net-pci设备的列表
    pci_get_device_structure(NETWORK_CLASS, ETHERNET_SUBCLASS, net_pdevs, &net_pdevs_count);
    //检测其中的virt-io-net设备
    for(int i = 0; i < net_pdevs_count;i++) {
        struct pci_device_structure_general_device_t *dev = net_pdevs[i];
        if(net_pdevs[i]->Vendor_ID==0x1AF4 && net_pdevs[i]->Device_ID>=0x1000 && net_pdevs[i]->Device_ID<=0x103F && dev->Subsystem_ID==1)
        { 
            virtio_net_pdev=net_pdevs[i];
            virtio_net_pdev_count++;
            break;
        }
    }
    if (virtio_net_pdev_count == 0) {
        kwarn("There is no virtio-net device in this computer!");
        return NOT_FOUND_DEVICE;
    }
      if (virtio_net_pdev->Command==0) {
        kwarn("The virtio-net device isn't support mmio!");
        return NOT_SUPPORTE_MMIO;
    }
    *bus=virtio_net_pdev->bus;
    *device=virtio_net_pdev->device;
    *function=virtio_net_pdev->func;
}