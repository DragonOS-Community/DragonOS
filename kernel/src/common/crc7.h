#pragma once
#include <common/sys/types.h>

/**
 * @brief 计算crc7
 *
 * @param crc crc初始值
 * @param buffer 输入缓冲区
 * @param len buffer大小（bytes）
 * @return uint8_t crc
 */
uint8_t crc7(uint8_t crc, const uint8_t *buffer, size_t len);