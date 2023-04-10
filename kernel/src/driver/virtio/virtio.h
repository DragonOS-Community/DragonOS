#pragma once
#include <common/glib.h>

//寻找并加载所有virtio设备的驱动（目前只有virtio-net，但其他virtio设备后续也可添加）
void  c_virtio_probe();
