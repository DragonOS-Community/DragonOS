#pragma once
#include <common/sys/types.h>

/**
 * @brief 计算crc16
 *
 * @param crc crc初始值
 * @param buffer 输入缓冲区
 * @param len buffer大小（bytes）
 * @return uint16_t crc
 */
uint16_t crc16(uint16_t crc, const uint8_t *buffer, size_t len);