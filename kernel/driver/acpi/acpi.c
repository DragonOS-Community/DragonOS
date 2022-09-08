#include "acpi.h"
#include <common/printk.h>
#include <common/kprint.h>
#include <driver/multiboot2/multiboot2.h>
#include <mm/mm.h>
#include <mm/mmio.h>

#define acpi_get_RSDT_entry_vaddr(phys_addr) (acpi_description_header_base + (phys_addr)-acpi_RSDT_entry_phys_base) // 获取RSDT entry的虚拟地址
// #define acpi_get_XSDT_entry_vaddr(phys_addr) (ACPI_DESCRIPTION_HEDERS_BASE + (phys_addr)-acpi_XSDT_entry_phys_base) // 获取XSDT entry的虚拟地址

static struct acpi_RSDP_t *rsdpv1;
static struct acpi_RSDP_2_t *rsdpv2;
static struct acpi_RSDT_Structure_t *rsdt;
static struct acpi_XSDT_Structure_t *xsdt;

static struct multiboot_tag_old_acpi_t old_acpi;
static struct multiboot_tag_new_acpi_t new_acpi;

static ul acpi_RSDT_offset = 0;
static ul acpi_XSDT_offset = 0;
static uint acpi_RSDT_Entry_num = 0;
static uint acpi_XSDT_Entry_num = 0;

static ul acpi_RSDT_entry_phys_base = 0; // RSDT中的第一个entry所在物理页的基地址

static uint64_t acpi_madt_vaddr = 0;              // MADT的虚拟地址
static uint64_t acpi_rsdt_virt_addr_base = 0;     // RSDT的虚拟地址
static uint64_t acpi_description_header_base = 0; // RSDT中的第一个entry所在虚拟地址

// static ul acpi_XSDT_entry_phys_base = 0; // XSDT中的第一个entry所在物理页的基地址

/**
 * @brief 迭代器，用于迭代描述符头（位于ACPI标准文件的Table 5-29）
 * @param  _fun            迭代操作调用的函数
 * @param  _data           数据
 */
void acpi_iter_SDT(bool (*_fun)(const struct acpi_system_description_table_header_t *, void *),
                   void *_data)
{

    struct acpi_system_description_table_header_t *sdt_header;
    if (acpi_use_xsdt)
    {
        ul *ent = &(xsdt->Entry);
        for (int i = 0; i < acpi_XSDT_Entry_num; ++i)
        {
            mm_map_phys_addr(acpi_description_header_base + PAGE_2M_SIZE * i, (*(ent + i)) & PAGE_2M_MASK, PAGE_2M_SIZE, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD, false);
            sdt_header = (struct acpi_system_description_table_header_t *)((ul)(acpi_description_header_base + PAGE_2M_SIZE * i));

            if (_fun(sdt_header, _data) == true)
                return;
        }
    }
    else
    {
        uint *ent = &(rsdt->Entry);
        for (int i = 0; i < acpi_RSDT_Entry_num; ++i)
        {

            sdt_header = (struct acpi_system_description_table_header_t *)(acpi_get_RSDT_entry_vaddr((ul)(*(ent + i))));

            if (_fun(sdt_header, _data) == true)
                return;
        }
    }

    return;
}

/**
 * @brief 获取MADT信息 Multiple APIC Description Table
 *
 * @param _iter_data 要被迭代的信息的结构体
 * @param _data 返回的MADT的虚拟地址
 * @param count 返回数组的长度
 * @return true
 * @return false
 */
bool acpi_get_MADT(const struct acpi_system_description_table_header_t *_iter_data, void *_data)
{
    if (!(_iter_data->Signature[0] == 'A' && _iter_data->Signature[1] == 'P' && _iter_data->Signature[2] == 'I' && _iter_data->Signature[3] == 'C'))
        return false;
    //*(struct acpi_Multiple_APIC_Description_Table_t *)_data = *(struct acpi_Multiple_APIC_Description_Table_t *)_iter_data;
    // 返回MADT的虚拟地址
    *(ul *)_data = (ul)_iter_data;
    acpi_madt_vaddr = (ul)_iter_data;
    return true;
}

/**
 * @brief 获取HPET HPET_description_table
 *
 * @param _iter_data 要被迭代的信息的结构体
 * @param _data 返回的HPET表的虚拟地址
 * @return true
 * @return false
 */
bool acpi_get_HPET(const struct acpi_system_description_table_header_t *_iter_data, void *_data)
{
    if (!(_iter_data->Signature[0] == 'H' && _iter_data->Signature[1] == 'P' && _iter_data->Signature[2] == 'E' && _iter_data->Signature[3] == 'T'))
        return false;
    *(ul *)_data = (ul)_iter_data;
    return true;
}

/**
 * @brief 初始化acpi模块
 *
 */
// todo: 修复bug：当物理机上提供了rsdpv2之后，rsdpv1是不提供的（物理地址为0），因此需要手动判断rsdp的版本信息，然后做对应的解析。
void acpi_init()
{
    kinfo("Initializing ACPI...");

    // 获取物理地址
    int reserved;

    multiboot2_iter(multiboot2_get_acpi_old_RSDP, &old_acpi, &reserved);
    rsdpv1 = &(old_acpi.rsdp);

    multiboot2_iter(multiboot2_get_acpi_new_RSDP, &new_acpi, &reserved);
    rsdpv2 = &(new_acpi.rsdp);

    uint64_t paddr = 0;
    // An ACPI-compatible OS must use the XSDT if present
    if (rsdpv2->XsdtAddress != 0x00UL)
    {
        // 不要删除这段注释（因为还不确定是代码的bug，还是真机的bug）
        /*
        acpi_use_xsdt = true;
        ul xsdt_phys_base = rsdpv2->XsdtAddress & PAGE_2M_MASK;
        acpi_XSDT_offset = rsdpv2->XsdtAddress - xsdt_phys_base;
        mm_map_phys_addr(ACPI_XSDT_VIRT_ADDR_BASE, xsdt_phys_base, PAGE_2M_SIZE, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD, false);
        kdebug("XSDT mapped!");

        xsdt = (struct acpi_XSDT_Structure_t *)(ACPI_XSDT_VIRT_ADDR_BASE + acpi_XSDT_offset);
        // 计算RSDT Entry的数量
        kdebug("offset=%d", sizeof(xsdt->header));
        kdebug("xsdt sign=%s", xsdt->header.Signature);
        acpi_XSDT_Entry_num = (xsdt->header.Length - sizeof(xsdt->header)) / 8;

        printk_color(ORANGE, BLACK, "XSDT Length=%dbytes.\n", xsdt->header.Length);
        printk_color(ORANGE, BLACK, "XSDT Entry num=%d\n", acpi_XSDT_Entry_num);

        mm_map_phys_addr(ACPI_XSDT_VIRT_ADDR_BASE, xsdt_phys_base, xsdt->header.Length + PAGE_2M_SIZE, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD, false);
        // 映射所有的Entry的物理地址
        ul *ent = &(xsdt->Entry);
        for (int j = 0; j < acpi_XSDT_Entry_num; ++j)
        {
            kdebug("entry=%#018lx, virt=%#018lx", (*(ent + j)) & PAGE_2M_MASK, ACPI_XSDT_DESCRIPTION_HEDERS_BASE + PAGE_2M_SIZE * j);
            // 映射RSDT ENTRY的物理地址
            mm_map_phys_addr(ACPI_XSDT_DESCRIPTION_HEDERS_BASE + PAGE_2M_SIZE * j, (*(ent + j)) & PAGE_2M_MASK, PAGE_2M_SIZE, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD, false);
        }
        */

        // 由于解析XSDT出现问题。暂时只使用Rsdpv2的rsdt，但是这是不符合ACPI规范的！！！
        ul rsdt_phys_base = rsdpv2->rsdp1.RsdtAddress & PAGE_2M_MASK;
        acpi_RSDT_offset = rsdpv2->rsdp1.RsdtAddress - rsdt_phys_base;

        //申请mmio空间
        uint64_t size = 0;
        mmio_create(PAGE_2M_SIZE, VM_IO | VM_DONTCOPY, &acpi_rsdt_virt_addr_base, &size);

        //映射rsdt表
        paddr = (uint64_t)rsdt_phys_base;
        mm_map(&initial_mm, acpi_rsdt_virt_addr_base, PAGE_2M_SIZE, paddr);

        // rsdt表虚拟地址
        rsdt = (struct acpi_RSDT_Structure_t *)(acpi_rsdt_virt_addr_base + acpi_RSDT_offset);
        kdebug("RSDT mapped!(v2)");

        // 计算RSDT Entry的数量
        kdebug("offset=%d", sizeof(rsdt->header));
        acpi_RSDT_Entry_num = (rsdt->header.Length - 36) / 4;

        printk_color(ORANGE, BLACK, "RSDT Length=%dbytes.\n", rsdt->header.Length);
        printk_color(ORANGE, BLACK, "RSDT Entry num=%d\n", acpi_RSDT_Entry_num);

        //申请mmio空间
        mmio_create(PAGE_2M_SIZE, VM_IO | VM_DONTCOPY, &acpi_description_header_base, &size);

        // 映射所有的Entry的物理地址
        acpi_RSDT_entry_phys_base = ((ul)(rsdt->Entry)) & PAGE_2M_MASK;
        // 由于地址只是32bit的，并且存在脏数据，这里需要手动清除高32bit，否则会触发#GP
        acpi_RSDT_entry_phys_base = MASK_HIGH_32bit(acpi_RSDT_entry_phys_base);

        paddr = (uint64_t)acpi_RSDT_entry_phys_base;
        mm_map(&initial_mm, acpi_description_header_base, PAGE_2M_SIZE, paddr);
    }
    else if (rsdpv1->RsdtAddress != (uint)0x00UL)
    {
        // rsdt表物理地址
        ul rsdt_phys_base = rsdpv1->RsdtAddress & PAGE_2M_MASK;
        acpi_RSDT_offset = rsdpv1->RsdtAddress - rsdt_phys_base;
        
        kdebug("rsdpv1->RsdtAddress=%#018lx", rsdpv1->RsdtAddress);
        //申请mmio空间
        uint64_t size = 0;
        mmio_create(PAGE_2M_SIZE, VM_IO | VM_DONTCOPY, &acpi_rsdt_virt_addr_base, &size);

        // kdebug("acpi_rsdt_virt_addr_base = %#018lx,size= %#010lx", acpi_rsdt_virt_addr_base, size);
        //映射rsdt表
        paddr = (uint64_t)rsdt_phys_base;
        mm_map(&initial_mm, acpi_rsdt_virt_addr_base, PAGE_2M_SIZE, paddr);
        // rsdt表虚拟地址
        rsdt = (struct acpi_RSDT_Structure_t *)(acpi_rsdt_virt_addr_base + acpi_RSDT_offset);
        kdebug("RSDT mapped!");

        // kdebug("length = %d",rsdt->header.Length);
        // 计算RSDT Entry的数量
        // kdebug("offset=%d", sizeof(rsdt->header));

        acpi_RSDT_Entry_num = (rsdt->header.Length - 36) / 4;

        printk_color(ORANGE, BLACK, "RSDT Length=%dbytes.\n", rsdt->header.Length);
        printk_color(ORANGE, BLACK, "RSDT Entry num=%d\n", acpi_RSDT_Entry_num);

        //申请mmio空间
        mmio_create(PAGE_2M_SIZE, VM_IO | VM_DONTCOPY, &acpi_description_header_base, &size);

        // 映射所有的Entry的物理地址
        acpi_RSDT_entry_phys_base = ((ul)(rsdt->Entry)) & PAGE_2M_MASK;
        // 由于地址只是32bit的，并且存在脏数据，这里需要手动清除高32bit，否则会触发#GP
        acpi_RSDT_entry_phys_base = MASK_HIGH_32bit(acpi_RSDT_entry_phys_base);

        paddr = (uint64_t)acpi_RSDT_entry_phys_base;
        mm_map(&initial_mm, acpi_description_header_base, PAGE_2M_SIZE, paddr);
        // kinfo("entry mapped!");
        
    }
    else
    {
        // should not reach here!
        kBUG("At acpi_init(): Cannot get right SDT!");
        while (1)
            ;
    }

    kinfo("ACPI module initialized!");
    return;
}
