#include "ahci.h"
#include "../../../common/kprint.h"
#include "../../../mm/slab.h"

struct pci_device_structure_header_t *ahci_devs[MAX_AHCI_DEVICES];

uint32_t count_ahci_devices = 0;

uint64_t ahci_port_base_vaddr; // 端口映射base addr

static void start_cmd(HBA_PORT *port);
static void stop_cmd(HBA_PORT *port);
static void port_rebase(HBA_PORT *port, int portno);

// Find a free command list slot
static int ahci_find_cmdslot(HBA_PORT *port);

// 计算HBA_MEM的虚拟内存地址
#define cal_HBA_MEM_VIRT_ADDR(device_num) (AHCI_MAPPING_BASE + (ul)(((struct pci_device_structure_general_device_t *)(ahci_devs[device_num]))->BAR5 - ((((struct pci_device_structure_general_device_t *)(ahci_devs[0]))->BAR5) & PAGE_2M_MASK)))
/**
 * @brief 初始化ahci模块
 *
 */
void ahci_init()
{
    pci_get_device_structure(0x1, 0x6, ahci_devs, &count_ahci_devices);

    kdebug("phys addr=%#018lx", (ul)(((struct pci_device_structure_general_device_t *)(ahci_devs[0]))->BAR5));
    // 映射ABAR
    mm_map_phys_addr(AHCI_MAPPING_BASE, ((ul)(((struct pci_device_structure_general_device_t *)(ahci_devs[0]))->BAR5)) & PAGE_2M_MASK, PAGE_2M_SIZE, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD);
    kdebug("ABAR mapped!");

    for (int i = 0; i < count_ahci_devices; ++i)
    {
        kdebug("[%d]  class_code=%d, sub_class=%d, progIF=%d, ABAR=%#010lx", i, ahci_devs[i]->Class_code, ahci_devs[i]->SubClass, ahci_devs[i]->ProgIF, ((struct pci_device_structure_general_device_t *)(ahci_devs[i]))->BAR5);
        // 赋值HBA_MEM结构体
        ahci_devices[i].dev_struct = ahci_devs[i];
        ahci_devices[i].hba_mem = (HBA_MEM *)(cal_HBA_MEM_VIRT_ADDR(i));
    }
    ahci_port_base_vaddr = (uint64_t)kmalloc(1048576, 0);
    ahci_probe_port(0);
    port_rebase(&ahci_devices[0].hba_mem->ports[0], 0);
    uint64_t buf[100];
    bool res = ahci_read(&(ahci_devices[0].hba_mem->ports[0]), 0, 0, 1, (uint64_t)&buf);
    kdebug("res=%d, buf[0]=%#010lx", (uint)res, (uint32_t)buf[0]);
    
}

// Check device type
static int check_type(HBA_PORT *port)
{
    uint32_t ssts = port->ssts;

    uint8_t ipm = (ssts >> 8) & 0x0F;
    uint8_t det = ssts & 0x0F;

    if (det != HBA_PORT_DET_PRESENT) // Check drive status
        return AHCI_DEV_NULL;
    if (ipm != HBA_PORT_IPM_ACTIVE)
        return AHCI_DEV_NULL;

    switch (port->sig)
    {
    case SATA_SIG_ATAPI:
        return AHCI_DEV_SATAPI;
    case SATA_SIG_SEMB:
        return AHCI_DEV_SEMB;
    case SATA_SIG_PM:
        return AHCI_DEV_PM;
    default:
        return AHCI_DEV_SATA;
    }
}

/**
 * @brief 检测端口连接的设备的类型
 *
 * @param device_num ahci设备号
 */
void ahci_probe_port(const uint32_t device_num)
{
    HBA_MEM *abar = ahci_devices[device_num].hba_mem;
    uint32_t pi = abar->pi;

    for (int i = 0; i < 32; ++i, (pi >>= 1))
    {
        if (pi & 1)
        {
            uint dt = check_type(&abar->ports[i]);
            ahci_devices[i].type = dt;
            if (dt == AHCI_DEV_SATA)
            {
                kdebug("SATA drive found at port %d", i);
            }
            else if (dt == AHCI_DEV_SATAPI)
            {
                kdebug("SATAPI drive found at port %d", i);
            }
            else if (dt == AHCI_DEV_SEMB)
            {
                kdebug("SEMB drive found at port %d", i);
            }
            else if (dt == AHCI_DEV_PM)
            {
                kdebug("PM drive found at port %d", i);
            }
            else
            {
                kdebug("No drive found at port %d", i);
            }
        }
    }
}

// Start command engine
static void start_cmd(HBA_PORT *port)
{
    // Wait until CR (bit15) is cleared
    while (port->cmd & HBA_PxCMD_CR)
        ;

    // Set FRE (bit4) and ST (bit0)
    port->cmd |= HBA_PxCMD_FRE;
    port->cmd |= HBA_PxCMD_ST;
}

// Stop command engine
static void stop_cmd(HBA_PORT *port)
{
    // Clear ST (bit0)
    port->cmd &= ~HBA_PxCMD_ST;

    // Clear FRE (bit4)
    port->cmd &= ~HBA_PxCMD_FRE;

    // Wait until FR (bit14), CR (bit15) are cleared
    while (1)
    {
        if (port->cmd & HBA_PxCMD_FR)
            continue;
        if (port->cmd & HBA_PxCMD_CR)
            continue;
        break;
    }
}

static void port_rebase(HBA_PORT *port, int portno)
{

    // Before rebasing Port memory space, OS must wait for current pending commands to finish
    // and tell HBA to stop receiving FIS from the port. Otherwise an accidently incoming FIS may be
    // written into a partially configured memory area.

    stop_cmd(port); // Stop command engine

    // Command list offset: 1K*portno
    // Command list entry size = 32
    // Command list entry maxim count = 32
    // Command list maxim size = 32*32 = 1K per port

    port->clb = ahci_port_base_vaddr + (portno << 10);

    memset((void *)(port->clb), 0, 1024);

    // FIS offset: 32K+256*portno
    // FIS entry size = 256 bytes per port
    port->fb = ahci_port_base_vaddr + (32 << 10) + (portno << 8);

    memset((void *)(port->fb), 0, 256);

    // Command table offset: 40K + 8K*portno
    // Command table size = 256*32 = 8K per port
    HBA_CMD_HEADER *cmdheader = (HBA_CMD_HEADER *)(port->clb);
    for (int i = 0; i < 32; ++i)
    {
        cmdheader[i].prdtl = 8; // 8 prdt entries per command table
                                // 256 bytes per command table, 64+16+48+16*8
        // Command table offset: 40K + 8K*portno + cmdheader_index*256
        cmdheader[i].ctba = ahci_port_base_vaddr + (40 << 10) + (portno << 13) + (i << 8);

        memset((void *)cmdheader[i].ctba, 0, 256);
    }

    start_cmd(port); // Start command engine
}

/**
 * @brief read data from SATA device using 48bit LBA address
 *
 * @param port HBA PORT
 * @param startl low 32bits of start addr
 * @param starth high 32bits of start addr
 * @param count total sectors to read
 * @param buf buffer
 * @return true done
 * @return false failed
 */
bool ahci_read(HBA_PORT *port, uint32_t startl, uint32_t starth, uint32_t count, uint64_t buf)
{
    port->is = (uint32_t)-1; // Clear pending interrupt bits
    int spin = 0;            // Spin lock timeout counter
    int slot = ahci_find_cmdslot(port);
    
    if (slot == -1)
        return false;
    
    HBA_CMD_HEADER *cmdheader = (HBA_CMD_HEADER *)port->clb;
    cmdheader += slot;
    cmdheader->cfl = sizeof(FIS_REG_H2D) / sizeof(uint32_t); // Command FIS size
    cmdheader->w = 0;                                        // Read from device
    cmdheader->prdtl = (uint16_t)((count - 1) >> 4) + 1;     // PRDT entries count

    HBA_CMD_TBL *cmdtbl = (HBA_CMD_TBL *)(cmdheader->ctba);
    memset(cmdtbl, 0, sizeof(HBA_CMD_TBL) + (cmdheader->prdtl - 1) * sizeof(HBA_PRDT_ENTRY));


    // 8K bytes (16 sectors) per PRDT
    int i;
    for (i = 0; i < cmdheader->prdtl - 1; ++i)
    {
        cmdtbl->prdt_entry[i].dba = buf;
        cmdtbl->prdt_entry[i].dbc = 8 * 1024 - 1; // 8K bytes (this value should always be set to 1 less than the actual value)
        cmdtbl->prdt_entry[i].i = 1;
        buf += 4 * 1024; // 4K words
        count -= 16;     // 16 sectors
    }

    // Last entry
    cmdtbl->prdt_entry[i].dba = buf;
    cmdtbl->prdt_entry[i].dbc = (count << 9) - 1; // 512 bytes per sector
    cmdtbl->prdt_entry[i].i = 1;

    // Setup command
    FIS_REG_H2D *cmdfis = (FIS_REG_H2D *)(&cmdtbl->cfis);

    cmdfis->fis_type = FIS_TYPE_REG_H2D;
    cmdfis->c = 1; // Command
    cmdfis->command = ATA_CMD_READ_DMA_EXT;

    cmdfis->lba0 = (uint8_t)startl;
    cmdfis->lba1 = (uint8_t)(startl >> 8);
    cmdfis->lba2 = (uint8_t)(startl >> 16);
    cmdfis->device = 1 << 6; // LBA mode

    cmdfis->lba3 = (uint8_t)(startl >> 24);
    cmdfis->lba4 = (uint8_t)starth;
    cmdfis->lba5 = (uint8_t)(starth >> 8);

    cmdfis->countl = count & 0xFF;
    cmdfis->counth = (count >> 8) & 0xFF;

    // The below loop waits until the port is no longer busy before issuing a new command
    while ((port->tfd & (ATA_DEV_BUSY | ATA_DEV_DRQ)) && spin < 1000000)
    {
        spin++;
    }
    if (spin == 1000000)
    {
        kerror("Port is hung");
        return false;
    }

    kdebug("slot=%d", slot);
    port->ci = 1 << slot; // Issue command

    // Wait for completion
    while (1)
    {
        // In some longer duration reads, it may be helpful to spin on the DPS bit
        // in the PxIS port field as well (1 << 5)
        if ((port->ci & (1 << slot)) == 0)
            break;
        if (port->is & HBA_PxIS_TFES) // Task file error
        {
            kerror("Read disk error");
            return false;
        }
    }

    // Check again
    if (port->is & HBA_PxIS_TFES)
    {
        kerror("Read disk error");
        return false;
    }

    return true;
}

// Find a free command list slot
static int ahci_find_cmdslot(HBA_PORT *port)
{
    // If not set in SACT and CI, the slot is free
    uint32_t slots = (port->sact | port->ci);
    int num_of_cmd_clots = (ahci_devices[0].hba_mem->cap&0x0f00)>>8;    // bit 12-8
    for (int i = 0; i < num_of_cmd_clots; i++)
    {
        if ((slots & 1) == 0)
            return i;
        slots >>= 1;
    }
    kerror("Cannot find free command list entry");
    return -1;
}