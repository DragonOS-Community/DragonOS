#include "acpi.h"
#include "../../common/printk.h"
#include "../../common/kprint.h"
#include "../multiboot2/multiboot2.h"

static struct acpi_RSDP_t *rsdp;
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
    struct multiboot_tag_old_acpi_t* tmp1;
    int reserved;
    kdebug("yyyy");
    multiboot2_iter(multiboot2_get_acpi_old_RSDP, tmp1, &reserved);
    kdebug("1");
    *rsdp = *(struct acpi_RSDP_t*)(tmp1->rsdp);

    kdebug("RsdtAddress=%#018lx", rsdp->RsdtAddress);

}