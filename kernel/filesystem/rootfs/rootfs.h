#pragma once

void rootfs_init();

/**
 * @brief 当磁盘文件系统被成功挂载后，释放rootfs所占的空间
 * 
 */
void rootfs_umount();