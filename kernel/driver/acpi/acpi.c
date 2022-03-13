#include "acpi.h"
#include "../../common/printk.h"
#include "../../common/kprint.h"
#include "../multiboot2/multiboot2.h"
#include "../../mm/mm.h"

static struct acpi_RSDP_t *rsdpv1;
static struct acpi_RSDP_2_t *rsdpv2;
static struct acpi_RSDT_Structure_t *rsdt;

static ul acpi_RSDT_offset = 0;
static uint acpi_RSDT_Entry_num = 0;
/**
 * @brief 迭代器，用于迭代描述符头（位于ACPI标准文件的Table 5-29）
 * @param  _fun            迭代操作调用的函数
 * @param  _data           数据
 */
void acpi_iter_SDT(bool (*_fun)(const struct acpi_iter_SDT_header_t *, void *),
                   void *_data)
{
}

/**
 * @brief 初始化acpi模块
 *
 */
void acpi_init()
{
    kinfo("Initializing ACPI...");

    // 获取rsdp
    struct multiboot_tag_old_acpi_t tmp1;

    int reserved;
    multiboot2_iter(multiboot2_get_acpi_old_RSDP, &tmp1, &reserved);

    *rsdpv1 = (tmp1.rsdp);


    kdebug("Rsdt_phys_Address=%#018lx", rsdpv1->RsdtAddress);
    kdebug("RSDP_Revision=%d", rsdpv1->Revision);

    // 映射RSDT区域的物理地址到页表
    // 暂定字节数为2MB
    // 由于页表映射的原因，需要清除低21位地址，才能填入页表
    ul base = rsdpv1->RsdtAddress & (~(0x1fffff));
    acpi_RSDT_offset = rsdpv1->RsdtAddress - base;
    mm_map_phys_addr(ACPI_RSDT_VIRT_ADDR_BASE, base, PAGE_2M_SIZE, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD);
    kdebug("RSDT mapped!");

    struct multiboot_tag_new_acpi_t tmp2;
    multiboot2_iter(multiboot2_get_acpi_new_RSDP, &tmp2, &reserved);
    *rsdpv2 = tmp2.rsdp;
    kdebug("Rsdt_v2_phys_Address=%#018lx", rsdpv2->rsdp1.RsdtAddress);
    kdebug("RSDP_v2_Revision=%d", rsdpv2->rsdp1.Revision);

    rsdt = ACPI_RSDT_VIRT_ADDR_BASE + acpi_RSDT_offset;

    // 计算RSDT Entry的数量
    acpi_RSDT_Entry_num = (rsdt->header.Length - 32) / 4;
    
    printk_color(ORANGE, BLACK, "%s\n", rsdt->header.Signature);
    printk_color(ORANGE, BLACK, "RSDT Length=%dbytes.\n", rsdt->header.Length);
    printk_color(ORANGE, BLACK, "RSDT Entry num=%d\n", acpi_RSDT_Entry_num);
}