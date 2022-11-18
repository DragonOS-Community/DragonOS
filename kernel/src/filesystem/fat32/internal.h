#pragma once
#include <common/sys/types.h>

/**
 * @brief 根据簇号计算该簇的起始扇区号（LBA地址）
 *
 * @param first_data_sector 数据区的其实扇区号
 * @param sec_per_clus 每个簇的扇区数量
 * @param cluster 簇号
 * @return uint32_t LBA地址
 */
static inline uint32_t __fat32_calculate_LBA(uint32_t first_data_sector, uint32_t sec_per_clus, uint32_t cluster)
{
    return first_data_sector + (cluster - 2) * sec_per_clus;
}

/**
 * @brief 计算LBA地址所在的簇
 *
 * @param first_data_sector 数据区的其实扇区号
 * @param sec_per_clus 每个簇的扇区数量
 * @param LBA LBA地址
 * @return uint32_t 所在的簇
 */
static inline uint32_t __fat32_LBA_to_cluster(uint32_t first_data_sector, uint32_t sec_per_clus, uint32_t LBA)
{
    return ((LBA - first_data_sector) / sec_per_clus) + 2;
}