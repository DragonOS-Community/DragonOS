#pragma once

#include <common/kprint.h>
#include <mm/slab.h>
#include <syscall/syscall.h>
#include <syscall/syscall_num.h>
#include <sched/sched.h>
#include <common/string.h>
#include <common/block.h>
#include <debug/bug.h>
#include <driver/pci/pci.h>
#include <mm/mm.h>

// 计算HBA_MEM的虚拟内存地址
#define MAX_AHCI_DEVICES 100
#define AHCI_MAPPING_BASE SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE + AHCI_MAPPING_OFFSET

/// @brief 保留了对 pci设备获取 和 mm内存映射 的依赖
void ahci_cpp_init(uint32_t *count_ahci_devices, struct pci_device_structure_header_t *ahci_devs[MAX_AHCI_DEVICES], struct pci_device_structure_general_device_t *gen_devs[MAX_AHCI_DEVICES]);