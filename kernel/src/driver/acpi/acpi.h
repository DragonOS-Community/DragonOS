/**
 * 解析acpi信息的模块
 **/

#pragma once

#include <common/glib.h>
#include <mm/mm.h>

struct acpi_RSDP_t
{
    unsigned char Signature[8];
    unsigned char Checksum;
    unsigned char OEMID[6];

    unsigned char Revision;

    // 32bit physical address of the RSDT
    uint RsdtAddress;
} __attribute__((packed));

struct acpi_RSDP_2_t
{
    struct acpi_RSDP_t rsdp1;

    // fields below are only valid when the revision value is 2 or above
    // 表的长度（单位：字节）从offset=0开始算
    uint Length;
    // 64bit的XSDT的物理地址
    ul XsdtAddress;
    unsigned char ExtendedChecksum; // 整个表的checksum，包括了之前的checksum区域

    unsigned char Reserved[3];
} __attribute__((packed));

// 初始化acpi模块
void acpi_init();