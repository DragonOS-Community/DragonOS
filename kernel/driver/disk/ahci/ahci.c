#include "ahci.h"
#include "../../../common/kprint.h"
#include "../../../mm/slab.h"

struct pci_device_structure_header_t *ahci_devs[MAX_AHCI_DEVICES];

uint32_t count_ahci_devices = 0;

uint64_t ahci_port_base_vaddr; // 端口映射base addr

static void start_cmd(HBA_PORT *port);
static void stop_cmd(HBA_PORT *port);
static void port_rebase(HBA_PORT *port, int portno);
static long ahci_query_disk();

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

    // 映射ABAR
    mm_map_phys_addr(AHCI_MAPPING_BASE, ((ul)(((struct pci_device_structure_general_device_t *)(ahci_devs[0]))->BAR5)) & PAGE_2M_MASK, PAGE_2M_SIZE, PAGE_KERNEL_PAGE | PAGE_PWT | PAGE_PCD);
    //kdebug("ABAR mapped!");

    for (int i = 0; i < count_ahci_devices; ++i)
    {
        //kdebug("[%d]  class_code=%d, sub_class=%d, progIF=%d, ABAR=%#010lx", i, ahci_devs[i]->Class_code, ahci_devs[i]->SubClass, ahci_devs[i]->ProgIF, ((struct pci_device_structure_general_device_t *)(ahci_devs[i]))->BAR5);
        // 赋值HBA_MEM结构体
        ahci_devices[i].dev_struct = ahci_devs[i];
        ahci_devices[i].hba_mem = (HBA_MEM *)(cal_HBA_MEM_VIRT_ADDR(i));
    }
    // todo: 支持多个ahci控制器。
    ahci_port_base_vaddr = (uint64_t)kmalloc(1048576, 0);
    ahci_probe_port(0);
    port_rebase(&ahci_devices[0].hba_mem->ports[0], 0);

    // 初始化请求队列
    ahci_req_queue.in_service = NULL;
    list_init(&(ahci_req_queue.queue_list));
    ahci_req_queue.request_count = 0;
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
                // kdebug("No drive found at port %d", i);
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
static bool ahci_read(HBA_PORT *port, uint32_t startl, uint32_t starth, uint32_t count, uint64_t buf)
{
    port->is = (uint32_t)-1; // Clear pending interrupt bits
    int spin = 0;            // Spin lock timeout counter
    int slot = ahci_find_cmdslot(port);

    if (slot == -1)
        return E_NOEMPTYSLOT;

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
        buf += 4 * 1024; // 4K uint16_ts
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
        return E_PORT_HUNG;
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
            return E_TASK_FILE_ERROR;
        }
    }

    // Check again
    if (port->is & HBA_PxIS_TFES)
    {
        kerror("Read disk error");
        return E_TASK_FILE_ERROR;
    }

    return AHCI_SUCCESS;
}

static bool ahci_write(HBA_PORT *port, uint32_t startl, uint32_t starth, uint32_t count,
                       uint64_t buf)
{
    port->is = 0xffff; // Clear pending interrupt bits
    int slot = ahci_find_cmdslot(port);
    if (slot == -1)
        return E_NOEMPTYSLOT;

    HBA_CMD_HEADER *cmdheader = (HBA_CMD_HEADER *)port->clb;

    cmdheader += slot;
    cmdheader->cfl = sizeof(FIS_REG_H2D) / sizeof(uint32_t); // Command FIS size
    cmdheader->w = 1;
    cmdheader->c = 1;
    cmdheader->p = 1;
    cmdheader->prdtl = (uint16_t)((count - 1) >> 4) + 1; // PRDT entries count

    HBA_CMD_TBL *cmdtbl = (HBA_CMD_TBL *)(cmdheader->ctba);
    memset(cmdtbl, 0, sizeof(HBA_CMD_TBL) + (cmdheader->prdtl - 1) * sizeof(HBA_PRDT_ENTRY));

    int i = 0;
    for (i = 0; i < cmdheader->prdtl - 1; ++i)
    {
        cmdtbl->prdt_entry[i].dba = buf;
        cmdtbl->prdt_entry[i].dbc = 8 * 1024 - 1; // 8K bytes
        cmdtbl->prdt_entry[i].i = 0;
        buf += 4 * 1024; // 4K words
        count -= 16;     // 16 sectors
    }
    cmdtbl->prdt_entry[i].dba = buf;

    cmdtbl->prdt_entry[i].dbc = count << 9; // 512 bytes per sector
    cmdtbl->prdt_entry[i].i = 0;
    FIS_REG_H2D *cmdfis = (FIS_REG_H2D *)(&cmdtbl->cfis);
    cmdfis->fis_type = FIS_TYPE_REG_H2D;
    cmdfis->c = 1; // Command
    cmdfis->command = ATA_CMD_WRITE_DMA_EXT;
    cmdfis->lba0 = (uint8_t)startl;
    cmdfis->lba1 = (uint8_t)(startl >> 8);
    cmdfis->lba2 = (uint8_t)(startl >> 16);
    cmdfis->lba3 = (uint8_t)(startl >> 24);
    cmdfis->lba4 = (uint8_t)starth;
    cmdfis->lba5 = (uint8_t)(starth >> 8);

    cmdfis->device = 1 << 6; // LBA mode

    cmdfis->countl = count & 0xff;
    cmdfis->counth = count >> 8;
    //    printk("[slot]{%d}", slot);
    port->ci = 1; // Issue command
    while (1)
    {
        // In some longer duration reads, it may be helpful to spin on the DPS bit
        // in the PxIS port field as well (1 << 5)
        if ((port->ci & (1 << slot)) == 0)
            break;
        if (port->is & HBA_PxIS_TFES)
        { // Task file error
            kerror("Write disk error");
            return E_TASK_FILE_ERROR;
        }
    }
    if (port->is & HBA_PxIS_TFES)
    {
        kerror("Write disk error");
        return E_TASK_FILE_ERROR;
    }

    return AHCI_SUCCESS;
}

// Find a free command list slot
static int ahci_find_cmdslot(HBA_PORT *port)
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
static struct block_device_request_packet *ahci_make_request(long cmd, uint64_t base_addr, uint64_t count, uint64_t buffer, uint8_t ahci_ctrl_num, uint8_t port_num)
{
    struct block_device_request_packet *pack = (struct block_device_request_packet *)kmalloc(sizeof(struct block_device_request_packet), 0);

    list_init(&pack->list);

    // 由于ahci不需要中断即可读取磁盘，因此end handler为空
    switch (cmd)
    {
    case ATA_CMD_READ_DMA_EXT:
        pack->end_handler = NULL;
        pack->cmd = ATA_CMD_READ_DMA_EXT;
        break;
    case ATA_CMD_WRITE_DMA_EXT:
        pack->end_handler = NULL;
        pack->cmd = ATA_CMD_WRITE_DMA_EXT;
        break;
    default:
        pack->end_handler = NULL;
        pack->cmd = cmd;
        break;
    }

    pack->LBA_start = base_addr;
    pack->count = count;
    pack->buffer_vaddr = buffer;

    pack->ahci_ctrl_num = ahci_ctrl_num;
    pack->port_num = port_num;
    return pack;
}

/**
 * @brief 结束磁盘请求
 *
 */
static void ahci_end_request()
{
    kfree((uint64_t *)ahci_req_queue.in_service);
    ahci_req_queue.in_service = NULL;

    // 进行下一轮的磁盘请求 （由于未实现单独的io调度器，这里会造成长时间的io等待）
    if (ahci_req_queue.request_count>0)
        ahci_query_disk();
}

static long ahci_query_disk()
{
    struct block_device_request_packet *pack = container_of(list_next(&ahci_req_queue.queue_list), struct block_device_request_packet, list);
    ahci_req_queue.in_service = pack;
    list_del(&(ahci_req_queue.in_service->list));
    --ahci_req_queue.request_count;

    long ret_val;

    switch (pack->cmd)
    {
    case ATA_CMD_READ_DMA_EXT:
        ret_val = ahci_read(&(ahci_devices[pack->ahci_ctrl_num].hba_mem->ports[pack->port_num]), pack->LBA_start & 0xFFFFFFFF, ((pack->LBA_start) >> 32) & 0xFFFFFFFF, pack->count, pack->buffer_vaddr);
        break;
    case ATA_CMD_WRITE_DMA_EXT:
        ret_val = ahci_write(&(ahci_devices[pack->ahci_ctrl_num].hba_mem->ports[pack->port_num]), pack->LBA_start & 0xFFFFFFFF, ((pack->LBA_start) >> 32) & 0xFFFFFFFF, pack->count, pack->buffer_vaddr);
        break;
    default:
        kerror("Unsupport ahci command: %#05lx", pack->cmd);
        ret_val = E_UNSUPPORTED_CMD;
        break;
    }
    ahci_end_request();
    return ret_val;
}

/**
 * @brief 将请求包提交到io队列
 *
 * @param pack
 */
static void ahci_submit(struct block_device_request_packet *pack)
{
    list_append(&(ahci_req_queue.queue_list), &(pack->list));
    ++ahci_req_queue.request_count;

    if (ahci_req_queue.in_service == NULL) // 当前没有正在请求的io包，立即执行磁盘请求
        ahci_query_disk();
}

/**
 * @brief ahci驱动程序的传输函数
 *
 * @param cmd 控制命令
 * @param base_addr 48位LBA地址
 * @param count total sectors to read
 * @param buf 缓冲区线性地址
 * @param ahci_ctrl_num ahci控制器号
 * @param port_num ahci控制器端口号
 * @return long
 */
static long ahci_transfer(long cmd, uint64_t base_addr, uint64_t count, uint64_t buf, uint8_t ahci_ctrl_num, uint8_t port_num)
{
    struct block_device_request_packet *pack = NULL;

    if (cmd == ATA_CMD_READ_DMA_EXT || cmd == ATA_CMD_WRITE_DMA_EXT)
    {
        pack = ahci_make_request(cmd, base_addr, count, buf, ahci_ctrl_num, port_num);
        ahci_submit(pack);
    }
    else
        return E_UNSUPPORTED_CMD;

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
}
struct block_device_operation ahci_operation =
    {
        .open = ahci_open,
        .close = ahci_close,
        .ioctl = ahci_ioctl,
        .transfer = ahci_transfer,
};
