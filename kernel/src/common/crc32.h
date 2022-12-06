#pragma once
#include <common/sys/types.h>

/**
 * @brief 计算crc32
 *
 * @param crc crc初始值
 * @param buffer 输入缓冲区
 * @param len buffer大小（bytes）
 * @return uint32_t crc
 */
uint32_t crc32(uint32_t crc, const uint8_t *buffer, size_t len);