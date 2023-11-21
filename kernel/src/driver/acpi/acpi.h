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

struct acpi_system_description_table_header_t
{
    // The ascii string representation of the table header.
    unsigned char Signature[4];
    // 整个表的长度（单位：字节），包括了header，从偏移量0处开始
    uint Length;
    // The revision of the  structure corresponding to the signature field for this table.
    unsigned char Revision;
    // The entire table, including the checksum field, must add to zero to be considered valid.
    char Checksum;

    unsigned char OEMID[6];
    unsigned char OEM_Table_ID[8];
    uint OEMRevision;
    uint CreatorID;
    uint CreatorRevision;
} __attribute__((packed));

// =========== RSDT 结构 =============
struct acpi_RSDT_Structure_t
{
    // 通过RSDT的header->Length可以计算出entry的数量n
    // n = (length - 32)/4
    struct acpi_system_description_table_header_t header;

    // 一个包含了n个32bit物理地址的数组，指向了其他的description headers
    uint Entry;
};

// =========== XSDT 结构 =============
struct acpi_XSDT_Structure_t
{
    // 通过RSDT的header->Length可以计算出entry的数量n
    // n = (length - 36)/8
    struct acpi_system_description_table_header_t header;

    // 一个包含了n个64bit物理地址的数组，指向了其他的description headers
    ul Entry;
};

// 初始化acpi模块
void acpi_init();