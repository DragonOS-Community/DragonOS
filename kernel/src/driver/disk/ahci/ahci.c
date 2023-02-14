#include "ahci.h"
#include <common/kprint.h>
#include <mm/slab.h>
#include <syscall/syscall.h>
#include <syscall/syscall_num.h>
#include <sched/sched.h>
#include <common/string.h>
#include <common/block.h>
#include <filesystem/MBR.h>
#include <debug/bug.h>
#include <common/kthread.h>

struct pci_device_structure_header_t *ahci_devs[MAX_AHCI_DEVICES];

struct blk_gendisk ahci_gendisk0 = {0}; // 暂时硬性指定一个ahci_device
static int __first_port = -1;           // 临时用于存储 ahci控制器的第一个可用端口 的变量

static uint32_t count_ahci_devices = 0;

static uint64_t ahci_port_base_vaddr;     // 端口映射base addr
static uint64_t ahci_port_base_phys_addr; // 端口映射的物理基地址（ahci控制器的参数的地址都是物理地址）

static void start_cmd(HBA_PORT *port);
static void stop_cmd(HBA_PORT *port);
static void port_rebase(HBA_PORT *port, int portno);

// 计算HBA_MEM的虚拟内存地址
#define cal_HBA_MEM_VIRT_ADDR(device_num) (AHCI_MAPPING_BASE + (ul)(((struct pci_device_structure_general_device_t *)(ahci_devs[device_num]))->BAR5 - ((((struct pci_device_structure_general_device_t *)(ahci_devs[0]))->BAR5) & PAGE_2M_MASK)))

long ahci_open();
long ahci_close();
static long ahci_ioctl(long cmd, long arg);
static long ahci_transfer(struct blk_gendisk *gd, long cmd, uint64_t base_addr, uint64_t count, uint64_t buf);

struct block_device_operation ahci_operation =
    {
        .open = ahci_open,
        .close = ahci_close,
        .ioctl = ahci_ioctl,
        .transfer = ahci_transfer,
};

/**
 * @brief ahci驱动器在block_device中的私有数据结构体
 *
 */
struct ahci_blk_private_data
{
    uint16_t ahci_ctrl_num;                        // ahci控制器号
    uint16_t ahci_port_num;                        // ahci端口号
    struct MBR_disk_partition_table_t *part_table; // 分区表
};

/**
 * @brief 申请ahci设备的私有信息结构体
 *
 * @return struct ahci_blk_private_data* 申请到的私有信息结构体
 */
static struct ahci_blk_private_data *__alloc_private_data()
{
    struct ahci_blk_private_data *data = (struct ahci_blk_private_data *)kzalloc(sizeof(struct ahci_blk_private_data), 0);
    data->part_table = (struct MBR_disk_partition_table_t *)kzalloc(512, 0);
    return data;
}

/**
 * @brief 释放ahci设备的分区的私有信息结构体
 *
 * @param pdata 待释放的结构体
 * @return int 错误码
 */
static int __release_private_data(struct ahci_blk_private_data *pdata)
{
    kfree(pdata->part_table);
    kfree(pdata);
    return 0;
}

/**
 * @brief 初始化gendisk结构体(暂时只支持1个gendisk)
 *
 */
static int ahci_init_gendisk()
{
    memset(&ahci_gendisk0, 0, sizeof(ahci_gendisk0));
    strcpy(ahci_gendisk0.disk_name, "ahci0");
    ahci_gendisk0.flags = BLK_GF_AHCI;
    ahci_gendisk0.fops = &ahci_operation;
    mutex_init(&ahci_gendisk0.open_mutex);

    // 为存储分区结构，分配内存空间
    ahci_gendisk0.private_data = __alloc_private_data();
    // 读取分区表
    // 暂时假设全都是MBR分区表的
    // todo: 支持GPT

    ((struct ahci_blk_private_data *)ahci_gendisk0.private_data)->ahci_ctrl_num = 0;
    ((struct ahci_blk_private_data *)ahci_gendisk0.private_data)->ahci_port_num = __first_port;

    MBR_read_partition_table(&ahci_gendisk0, ((struct ahci_blk_private_data *)ahci_gendisk0.private_data)->part_table);

    struct MBR_disk_partition_table_t *ptable = ((struct ahci_blk_private_data *)ahci_gendisk0.private_data)->part_table;

    // 求出可用分区数量
    for (int i = 0; i < 4; ++i)
    {
        // 分区可用
        if (ptable->DPTE[i].type != 0)
            ++ahci_gendisk0.part_cnt;
    }
    if (ahci_gendisk0.part_cnt)
    {
        // 分配分区结构体数组的空间
        ahci_gendisk0.partition = (struct block_device *)kzalloc(ahci_gendisk0.part_cnt * sizeof(struct block_device), 0);
        int cnt = 0;
        // 循环遍历每个分区
        for (int i = 0; i < 4; ++i)
        {
            // 分区可用
            if (ptable->DPTE[i].type != 0)
            {
                // 初始化分区结构体
                ahci_gendisk0.partition[cnt].bd_disk = &ahci_gendisk0;
                ahci_gendisk0.partition[cnt].bd_partno = cnt;
                // FIXME 需要注释
                ahci_gendisk0.partition[cnt].bd_sectors_num = ptable->DPTE[i].total_sectors;
                ahci_gendisk0.partition[cnt].bd_start_sector = ptable->DPTE[i].starting_sector;
                ahci_gendisk0.partition[cnt].bd_superblock = NULL; // 挂载文件系统时才会初始化superblock
                ahci_gendisk0.partition[cnt].bd_start_LBA = ptable->DPTE[i].starting_LBA;
                ++cnt;
            }
        }
    }

    return 0;
};

/**
 * @brief 初始化ahci模块
 *
 */
void ahci_init()
{
    kinfo("Initializing AHCI...");
    pci_get_device_structure(0x1, 0x6, ahci_devs, &count_ahci_devices);

    if (count_ahci_devices == 0)
    {
        kwarn("There is no AHCI device found on this computer!");
        return;
    }
    // 映射ABAR
    kdebug("phys_2_virt(ahci_devs[0])= %#018lx", (ahci_devs[0]));
    kdebug("((struct pci_device_structure_general_device_t *)phys_2_virt(ahci_devs[0])))->BAR5= %#018lx", ((struct pci_device_structure_general_device_t *)(ahci_devs[0]))->BAR5);
    uint32_t bar5 = ((struct pci_device_structure_general_device_t *)(ahci_devs[0]))->BAR5;

    mm_map_phys_addr(AHCI_MAPPING_BASE, (ul)(bar5)&PAGE_2M_MASK, PAGE_2M_SIZE, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD, false);
    kdebug("ABAR mapped!");
    for (int i = 0; i < count_ahci_devices; ++i)
    {
        // kdebug("[%d]  class_code=%d, sub_class=%d, progIF=%d, ABAR=%#010lx", i, ahci_devs[i]->Class_code, ahci_devs[i]->SubClass, ahci_devs[i]->ProgIF, ((struct pci_device_structure_general_device_t *)(ahci_devs[i]))->BAR5);
        //  赋值HBA_MEM结构体
        ahci_devices[i].dev_struct = ahci_devs[i];
        ahci_devices[i].hba_mem = (HBA_MEM *)(cal_HBA_MEM_VIRT_ADDR(i));
        kdebug("ahci_devices[i].hba_mem = %#018lx", (ul)ahci_devices[i].hba_mem);
    }

    // todo: 支持多个ahci控制器。
    ahci_port_base_vaddr = (uint64_t)kmalloc(1048576, 0);
    kdebug("ahci_port_base_vaddr=%#018lx", ahci_port_base_vaddr);
    ahci_probe_port(0);

    BUG_ON(ahci_init_gendisk() != 0);
    kinfo("AHCI initialized.");
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
 * @param device_num ahci控制器号
 */
static void ahci_probe_port(const uint32_t device_num)
{
    HBA_MEM *abar = ahci_devices[device_num].hba_mem;
    uint32_t pi = abar->pi;

    for (int i = 0; i < 32; ++i, (pi >>= 1))
    {
        if (pi & 1)
        {
            uint dt = check_type(&abar->ports[i]);
            ahci_devices[i].type = dt;
            switch (dt)
            {
            case AHCI_DEV_SATA:
                kdebug("SATA drive found at port %d", i);
                goto found;
            case AHCI_DEV_SATAPI:
                kdebug("SATAPI drive found at port %d", i);
                goto found;
            case AHCI_DEV_SEMB:
                kdebug("SEMB drive found at port %d", i);
                goto found;
            case AHCI_DEV_PM:
                kdebug("PM drive found at port %d", i);
                goto found;
            found:;
                port_rebase(&ahci_devices[0].hba_mem->ports[i], i);
                if (__first_port == -1)
                    __first_port = i;
                break;
            default:
                kdebug("No drive found at port %d", i);
                break;
            }
        }
    }
}

// Start command engine
static void start_cmd(HBA_PORT *port)
{
    // Wait until CR (bit15) is cleared
    while ((port->cmd) & HBA_PxCMD_CR)
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

    port->clb = virt_2_phys(ahci_port_base_vaddr + (portno << 10));

    memset((void *)(phys_2_virt(port->clb)), 0, 1024);

    // FIS offset: 32K+256*portno
    // FIS entry size = 256 bytes per port
    port->fb = virt_2_phys(ahci_port_base_vaddr + (32 << 10) + (portno << 8));

    memset((void *)(phys_2_virt(port->fb)), 0, 256);

    // Command table offset: 40K + 8K*portno
    // Command table size = 256*32 = 8K per port
    HBA_CMD_HEADER *cmdheader = (HBA_CMD_HEADER *)(phys_2_virt(port->clb));
    for (int i = 0; i < 32; ++i)
    {
        cmdheader[i].prdtl = 8; // 8 prdt entries per command table
                                // 256 bytes per command table, 64+16+48+16*8
        // Command table offset: 40K + 8K*portno + cmdheader_index*256
        cmdheader[i].ctba = virt_2_phys((ahci_port_base_vaddr + (40 << 10) + (portno << 13) + (i << 8)));

        memset((void *)phys_2_virt(cmdheader[i].ctba), 0, 256);
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
 * @param ret_slot 执行命令的插槽号（传出参数）
 * @return true done
 * @return false failed
 */
static int ahci_read(HBA_PORT *port, uint32_t startl, uint32_t starth, uint32_t count, uint64_t buf, int8_t *ret_slot)
{
    port->is = (uint32_t)-1; // Clear pending interrupt bits

    int spin = 0; // Spin lock timeout counter
    int slot = ahci_find_cmdslot(port);

    if (slot == -1)
        return E_NOEMPTYSLOT;
    if (ret_slot)
    {
        *ret_slot = slot;
    }

    HBA_CMD_HEADER *cmdheader = (HBA_CMD_HEADER *)phys_2_virt(port->clb);
    cmdheader += slot;
    cmdheader->cfl = sizeof(FIS_REG_H2D) / sizeof(uint32_t); // Command FIS size
    cmdheader->w = 0;                                        // Read from device
    cmdheader->prdtl = (uint16_t)((count - 1) >> 4) + 1;     // PRDT entries count

    HBA_CMD_TBL *cmdtbl = (HBA_CMD_TBL *)phys_2_virt(cmdheader->ctba);
    memset(cmdtbl, 0, sizeof(HBA_CMD_TBL) + (cmdheader->prdtl - 1) * sizeof(HBA_PRDT_ENTRY));

    // 8K bytes (16 sectors) per PRDT
    int i;
    for (i = 0; i < cmdheader->prdtl - 1; ++i)
    {
        cmdtbl->prdt_entry[i].dba = virt_2_phys(buf);
        cmdtbl->prdt_entry[i].dbc = 8 * 1024 - 1; // 8K bytes (this value should always be set to 1 less than the actual value)
        cmdtbl->prdt_entry[i].i = 1;
        buf += 4 * 1024; // 4K uint16_ts
        count -= 16;     // 16 sectors
    }

    // Last entry
    cmdtbl->prdt_entry[i].dba = virt_2_phys(buf);
    cmdtbl->prdt_entry[i].dbc = (count << 9) - 1; // 512 bytes per sector
    cmdtbl->prdt_entry[i].i = 1;

    // Setup command
    FIS_REG_H2D *cmdfis = (FIS_REG_H2D *)(&cmdtbl->cfis);

    cmdfis->fis_type = FIS_TYPE_REG_H2D;
    cmdfis->c = 1; // Command
    cmdfis->command = AHCI_CMD_READ_DMA_EXT;

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
    while ((port->tfd & (AHCI_DEV_BUSY | AHCI_DEV_DRQ)) && spin < 1000000)
    {
        spin++;
    }
    if (spin == 1000000)
    {
        kerror("Port is hung");
        return E_PORT_HUNG;
    }

    port->ci = 1 << slot; // Issue command

    return 0;
}

/**
 * @brief 检查请求包是否已完成
 *
 * @param port_num HBA PORT 编号
 * @param ahci_ctrl_num ahci控制号
 * @param ret_slot 执行命令的插槽号
 * @param err 错误信息
 */
int ahci_check_complete(uint8_t port_num, uint8_t ahci_ctrl_num, int8_t slot, char *err)
{

    HBA_PORT *port = ahci_get_port(port_num, ahci_ctrl_num);
    int retval = -EBUSY;
    if (slot == -1)
        retval = -EINVAL;
    // In some longer duration reads, it may be helpful to spin on the DPS bit
    // in the PxIS port field as well (1 << 5)
    if ((port->ci & (1 << slot)) == 0)
        retval = 0;
    if (port->is & HBA_PxIS_TFES) // Task file error
    {
        if (err != NULL)
            kerror(*err);
        retval = E_TASK_FILE_ERROR;
    }
    return retval;
}

/**
 * @brief write data to SATA device using 48bit LBA address
 *
 * @param port HBA PORT
 * @param startl low 32bits of start addr
 * @param starth high 32bits of start addr
 * @param count total sectors to read
 * @param buf buffer
 * @param ret_slot 执行命令的插槽号（传出参数）
 * @return success 0
 */
static int ahci_write(HBA_PORT *port, uint32_t startl, uint32_t starth, uint32_t count,
                      uint64_t buf, int8_t *ret_slot)
{
    port->is = 0xffff; // Clear pending interrupt bits
    int slot = ahci_find_cmdslot(port);
    if (slot == -1)
        return E_NOEMPTYSLOT;
    if (ret_slot)
    {
        *ret_slot = slot;
    }
    HBA_CMD_HEADER *cmdheader = (HBA_CMD_HEADER *)phys_2_virt(port->clb);

    cmdheader += slot;
    cmdheader->cfl = sizeof(FIS_REG_H2D) / sizeof(uint32_t); // Command FIS size
    cmdheader->w = 1;
    cmdheader->c = 1;
    cmdheader->p = 1;
    cmdheader->prdtl = (uint16_t)((count - 1) >> 4) + 1; // PRDT entries count

    HBA_CMD_TBL *cmdtbl = (HBA_CMD_TBL *)phys_2_virt(cmdheader->ctba);
    memset(cmdtbl, 0, sizeof(HBA_CMD_TBL) + (cmdheader->prdtl - 1) * sizeof(HBA_PRDT_ENTRY));

    int i = 0;
    for (i = 0; i < cmdheader->prdtl - 1; ++i)
    {
        cmdtbl->prdt_entry[i].dba = virt_2_phys(buf);
        cmdtbl->prdt_entry[i].dbc = 8 * 1024 - 1; // 8K bytes
        cmdtbl->prdt_entry[i].i = 0;
        buf += 4 * 1024; // 4K words
        count -= 16;     // 16 sectors
    }
    cmdtbl->prdt_entry[i].dba = virt_2_phys(buf);

    cmdtbl->prdt_entry[i].dbc = count << 9; // 512 bytes per sector
    cmdtbl->prdt_entry[i].i = 0;
    FIS_REG_H2D *cmdfis = (FIS_REG_H2D *)(&cmdtbl->cfis);
    cmdfis->fis_type = FIS_TYPE_REG_H2D;
    cmdfis->c = 1; // Command
    cmdfis->command = AHCI_CMD_WRITE_DMA_EXT;
    cmdfis->lba0 = (uint8_t)startl;
    cmdfis->lba1 = (uint8_t)(startl >> 8);
    cmdfis->lba2 = (uint8_t)(startl >> 16);
    cmdfis->lba3 = (uint8_t)(startl >> 24);
    cmdfis->lba4 = (uint8_t)starth;
    cmdfis->lba5 = (uint8_t)(starth >> 8);

    cmdfis->device = 1 << 6; // LBA mode

    cmdfis->countl = count & 0xff;
    cmdfis->counth = count >> 8;
    port->ci = 1; // Issue command

    return 0;
}

// Find a free command list slot
int ahci_find_cmdslot(HBA_PORT *port)
{
    // If not set in SACT and CI, the slot is free
    uint32_t slots = (port->sact | port->ci);
    int num_of_cmd_clots = (ahci_devices[0].hba_mem->cap & 0x0f00) >> 8; // bit 12-8
    for (int i = 0; i < num_of_cmd_clots; i++)
    {
        if ((slots & 1) == 0)
            return i;
        slots >>= 1;
    }
    kerror("Cannot find free command list entry");
    return -1;
}

long ahci_open()
{
    return 0;
}

long ahci_close()
{
    return 0;
}

/**
 * @brief 创建ahci磁盘请求包
 *
 * @param cmd 控制命令
 * @param base_addr 48位LBA地址
 * @param count total sectors to read
 * @param buf 缓冲区线性地址
 * @param ahci_ctrl_num ahci控制器号
 * @param port_num ahci控制器端口号
 * @return struct block_device_request_packet*
 */
static struct ahci_request_packet_t *ahci_make_request(long cmd, uint64_t base_addr, uint64_t count, uint64_t buffer, uint8_t ahci_ctrl_num, uint8_t port_num)
{
    struct ahci_request_packet_t *pack = (struct ahci_request_packet_t *)kzalloc(sizeof(struct ahci_request_packet_t), 0);
    pack->blk_pak.device_type = BLK_TYPE_AHCI;

    // 由于ahci不需要中断即可读取磁盘，因此end handler为空
    switch (cmd)
    {
    case AHCI_CMD_READ_DMA_EXT:
        pack->blk_pak.end_handler = NULL;
        pack->blk_pak.cmd = AHCI_CMD_READ_DMA_EXT;
        break;
    case AHCI_CMD_WRITE_DMA_EXT:
        pack->blk_pak.end_handler = NULL;
        pack->blk_pak.cmd = AHCI_CMD_WRITE_DMA_EXT;
        break;
    default:
        pack->blk_pak.end_handler = NULL;
        pack->blk_pak.cmd = cmd;
        break;
    }

    pack->blk_pak.LBA_start = base_addr;
    pack->blk_pak.count = count;
    pack->blk_pak.buffer_vaddr = buffer;
    pack->ahci_ctrl_num = ahci_ctrl_num;
    pack->port_num = port_num;
    return pack;
}

long ahci_query_disk(struct ahci_request_packet_t *pack, int8_t *ret_slot)
{

    long ret_val = 0;
    switch (pack->blk_pak.cmd)
    {
    case AHCI_CMD_READ_DMA_EXT:
        ret_val = ahci_read(&(ahci_devices[pack->ahci_ctrl_num].hba_mem->ports[pack->port_num]), pack->blk_pak.LBA_start & 0xFFFFFFFF, ((pack->blk_pak.LBA_start) >> 32) & 0xFFFFFFFF, pack->blk_pak.count, pack->blk_pak.buffer_vaddr, ret_slot);
        break;
    case AHCI_CMD_WRITE_DMA_EXT:
        ret_val = ahci_write(&(ahci_devices[pack->ahci_ctrl_num].hba_mem->ports[pack->port_num]), pack->blk_pak.LBA_start & 0xFFFFFFFF, ((pack->blk_pak.LBA_start) >> 32) & 0xFFFFFFFF, pack->blk_pak.count, pack->blk_pak.buffer_vaddr, ret_slot);
        break;
    default:
        kerror("Unsupport ahci command: %#05lx", pack->blk_pak.cmd);
        ret_val = E_UNSUPPORTED_CMD;
        break;
    }

    return ret_val;
}

/**
 * @brief ahci驱动程序的传输函数
 *
 * @param gd 磁盘设备结构体
 * @param cmd 控制命令
 * @param base_addr 48位LBA地址
 * @param count total sectors to read
 * @param buf 缓冲区线性地址
 * @return long
 */
static long ahci_transfer(struct blk_gendisk *gd, long cmd, uint64_t base_addr, uint64_t count, uint64_t buf)
{
    struct ahci_request_packet_t *pack = NULL;
    struct ahci_blk_private_data *pdata = (struct ahci_blk_private_data *)gd->private_data;

    if (cmd == AHCI_CMD_READ_DMA_EXT || cmd == AHCI_CMD_WRITE_DMA_EXT)
    {
        pack = ahci_make_request(cmd, base_addr, count, buf, pdata->ahci_ctrl_num, pdata->ahci_port_num);
        ahci_push_request(pack);
    }
    else
    {
        kdebug("ahci_transfer: E_UNSUPPORTED_CMD");
        return E_UNSUPPORTED_CMD;
    }

    return AHCI_SUCCESS;
}

/**
 * @brief todo: io控制器函数
 *
 * @param cmd 命令
 * @param arg 参数
 * @return long
 */
static long ahci_ioctl(long cmd, long arg)
{
    return 0;
}

/**
 * @brief 根据端口号获取端口结构体
 * @param port_num 端口号
 * @param ahci_ctrl_num 控制号
 */
HBA_PORT *ahci_get_port(uint8_t port_num, uint8_t ahci_ctrl_num)
{
    return &(ahci_devices[ahci_ctrl_num].hba_mem->ports[port_num]);
}
