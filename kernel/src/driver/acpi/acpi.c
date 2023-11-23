#include "acpi.h"
#include <common/kprint.h>
#include <driver/multiboot2/multiboot2.h>

extern void rs_acpi_init(uint64_t rsdp_paddr1, uint64_t rsdp_paddr2);

static struct acpi_RSDP_t *rsdpv1;
static struct acpi_RSDP_2_t *rsdpv2;

static struct multiboot_tag_old_acpi_t old_acpi;
static struct multiboot_tag_new_acpi_t new_acpi;

/**
 * @brief 初始化acpi模块
 *
 */
void acpi_init()
{
    kinfo("Initializing ACPI...");

    // 获取物理地址
    int reserved;

    multiboot2_iter(multiboot2_get_acpi_old_RSDP, &old_acpi, &reserved);
    rsdpv1 = &(old_acpi.rsdp);

    multiboot2_iter(multiboot2_get_acpi_new_RSDP, &new_acpi, &reserved);
    rsdpv2 = &(new_acpi.rsdp);

    // rsdpv1、rsdpv2，二者有一个能成功即可
    rs_acpi_init((uint64_t)rsdpv1, (uint64_t)rsdpv2);

    kinfo("ACPI module initialized!");
    return;
}
