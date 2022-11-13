#pragma once
#include <common/sys/types.h>

/**
 * @brief 计算crc64
 *
 * @param crc crc初始值
 * @param buffer 输入缓冲区
 * @param len buffer大小（bytes）
 * @return uint64_t crc
 */
uint64_t crc64(uint64_t crc, const uint8_t *buffer, size_t len);