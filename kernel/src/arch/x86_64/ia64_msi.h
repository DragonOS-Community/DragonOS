#pragma once

#include <driver/pci/msi.h>

/**
 * @brief 生成msi消息
 * 
 * @param msi_desc msi描述符
 * @return struct msi_msg_t* msi消息指针（在描述符内）
 */
struct msi_msg_t *msi_arch_get_msg(struct msi_desc_t *msi_desc);