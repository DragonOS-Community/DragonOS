#pragma once
#include <common/glib.h>

#define GET_VIRTADDRESS_SUCCESS 0
#define NOT_FOUND_DEVICE 1
#define NOT_SUPPORTE_MMIO 2
#define GET_VIRTADDRESS_FAILURE 3

// 获取virtio-net 设备
uint8_t get_virtio_net_device(uint8_t * bus, uint8_t *device,uint8_t * function);
//寻找并加载所有virtio设备的驱动（目前只有virtio-net，但其他virtio设备后续也可添加）
void  c_virtio_probe();
