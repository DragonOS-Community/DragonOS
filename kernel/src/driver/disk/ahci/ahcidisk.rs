use super::{_port, hba::HbaCmdTable, virt_2_phys};
use crate::driver::base::block::block_device::{BlockDevice, BlockId};
use crate::driver::base::block::disk_info::Partition;
use crate::driver::base::block::SeekFrom;
use crate::driver::base::device::bus::Bus;

use crate::driver::base::device::driver::Driver;
use crate::driver::base::device::{Device, DeviceType, IdTable};
use crate::driver::base::kobject::{KObjType, KObject, KObjectState};
use crate::driver::base::kset::KSet;
use crate::driver::disk::ahci::HBA_PxIS_TFES;

use crate::filesystem::kernfs::KernFSInode;
use crate::filesystem::mbr::MbrDiskPartionTable;

use crate::kdebug;
use crate::libs::rwlock::{RwLockReadGuard, RwLockWriteGuard};
use crate::libs::{spinlock::SpinLock, vec_cursor::VecCursor};
use crate::mm::{phys_2_virt, verify_area, VirtAddr};
use crate::syscall::SystemError;
use crate::{
    driver::disk::ahci::hba::{
        FisRegH2D, FisType, HbaCmdHeader, ATA_CMD_READ_DMA_EXT, ATA_CMD_WRITE_DMA_EXT,
        ATA_DEV_BUSY, ATA_DEV_DRQ,
    },
    kerror,
};

use alloc::sync::Weak;
use alloc::{string::String, sync::Arc, vec::Vec};

use core::fmt::Debug;
use core::sync::atomic::{compiler_fence, Ordering};
use core::{mem::size_of, ptr::write_bytes};

/// @brief: 只支持MBR分区格式的磁盘结构体
pub struct AhciDisk {
    pub name: String,
    pub flags: u16,                      // 磁盘的状态flags
    pub partitions: Vec<Arc<Partition>>, // 磁盘分区数组
    // port: &'static mut HbaPort,      // 控制硬盘的端口
    pub ctrl_num: u8,
    pub port_num: u8,
    /// 指向LockAhciDisk的弱引用
    self_ref: Weak<LockedAhciDisk>,
}

/// @brief: 带锁的AhciDisk
#[derive(Debug)]
pub struct LockedAhciDisk(pub SpinLock<AhciDisk>);
/// 函数实现
impl Debug for AhciDisk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{{ name: {}, flags: {}, part_s: {:?} }}",
            self.name, self.flags, self.partitions
        )?;
        return Ok(());
    }
}

impl AhciDisk {
    fn read_at(
        &self,
        lba_id_start: BlockId, // 起始lba编号
        count: usize,          // 读取lba的数量
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        assert!((buf.len() & 511) == 0);
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        let check_length = ((count - 1) >> 4) + 1; // prdt length
        if count * 512 > buf.len() || check_length > 8 as usize {
            kerror!("ahci read: e2big");
            // 不可能的操作
            return Err(SystemError::E2BIG);
        } else if count == 0 {
            return Ok(0);
        }

        let port = _port(self.ctrl_num, self.port_num);
        volatile_write!(port.is, u32::MAX); // Clear pending interrupt bits

        let slot = port.find_cmdslot().unwrap_or(u32::MAX);

        if slot == u32::MAX {
            return Err(SystemError::EIO);
        }

        #[allow(unused_unsafe)]
        let cmdheader: &mut HbaCmdHeader = unsafe {
            (phys_2_virt(
                volatile_read!(port.clb) as usize
                    + slot as usize * size_of::<HbaCmdHeader>() as usize,
            ) as *mut HbaCmdHeader)
                .as_mut()
                .unwrap()
        };

        cmdheader.cfl = (size_of::<FisRegH2D>() / size_of::<u32>()) as u8;

        volatile_set_bit!(cmdheader.cfl, 1 << 6, false); //  Read/Write bit : Read from device
        volatile_write!(cmdheader.prdtl, check_length as u16); // PRDT entries count

        // 设置数据存放地址
        let mut buf_ptr = buf as *mut [u8] as *mut usize as usize;

        // 由于目前的内存管理机制无法把用户空间的内存地址转换为物理地址，所以只能先把数据拷贝到内核空间
        // TODO：在内存管理重构后，可以直接使用用户空间的内存地址

        let user_buf = verify_area(VirtAddr::new(buf_ptr as usize), buf.len()).is_ok();
        let mut kbuf = if user_buf {
            let mut x: Vec<u8> = Vec::new();
            x.resize(buf.len(), 0);
            Some(x)
        } else {
            None
        };

        if kbuf.is_some() {
            buf_ptr = kbuf.as_mut().unwrap().as_mut_ptr() as usize;
        }

        #[allow(unused_unsafe)]
        let cmdtbl = unsafe {
            (phys_2_virt(volatile_read!(cmdheader.ctba) as usize) as *mut HbaCmdTable)
                .as_mut()
                .unwrap() // 必须使用 as_mut ，得到的才是原来的变量
        };
        let mut tmp_count = count;

        unsafe {
            // 清空整个table的旧数据
            write_bytes(cmdtbl, 0, 1);
        }
        // kdebug!("cmdheader.prdtl={}", volatile_read!(cmdheader.prdtl));

        // 8K bytes (16 sectors) per PRDT
        for i in 0..((volatile_read!(cmdheader.prdtl) - 1) as usize) {
            volatile_write!(cmdtbl.prdt_entry[i].dba, virt_2_phys(buf_ptr) as u64);
            cmdtbl.prdt_entry[i].dbc = 8 * 1024 - 1;
            volatile_set_bit!(cmdtbl.prdt_entry[i].dbc, 1 << 31, true); // 允许中断 prdt_entry.i
            buf_ptr += 8 * 1024;
            tmp_count -= 16;
        }

        // Last entry
        let las = (volatile_read!(cmdheader.prdtl) - 1) as usize;
        volatile_write!(cmdtbl.prdt_entry[las].dba, virt_2_phys(buf_ptr) as u64);
        cmdtbl.prdt_entry[las].dbc = ((tmp_count << 9) - 1) as u32; // 数据长度

        volatile_set_bit!(cmdtbl.prdt_entry[las].dbc, 1 << 31, true); // 允许中断

        // 设置命令
        let cmdfis = unsafe {
            ((&mut cmdtbl.cfis) as *mut [u8] as *mut usize as *mut FisRegH2D)
                .as_mut()
                .unwrap()
        };
        volatile_write!(cmdfis.fis_type, FisType::RegH2D as u8);
        volatile_set_bit!(cmdfis.pm, 1 << 7, true); // command_bit set
        volatile_write!(cmdfis.command, ATA_CMD_READ_DMA_EXT);

        volatile_write!(cmdfis.lba0, (lba_id_start & 0xFF) as u8);
        volatile_write!(cmdfis.lba1, ((lba_id_start >> 8) & 0xFF) as u8);
        volatile_write!(cmdfis.lba2, ((lba_id_start >> 16) & 0xFF) as u8);
        volatile_write!(cmdfis.lba3, ((lba_id_start >> 24) & 0xFF) as u8);
        volatile_write!(cmdfis.lba4, ((lba_id_start >> 32) & 0xFF) as u8);
        volatile_write!(cmdfis.lba5, ((lba_id_start >> 40) & 0xFF) as u8);

        volatile_write!(cmdfis.countl, (count & 0xFF) as u8);
        volatile_write!(cmdfis.counth, ((count >> 8) & 0xFF) as u8);

        volatile_write!(cmdfis.device, 1 << 6); // LBA Mode

        // 等待之前的操作完成
        let mut spin_count = 0;
        const SPIN_LIMIT: u32 = 10000;

        while (volatile_read!(port.tfd) as u8 & (ATA_DEV_BUSY | ATA_DEV_DRQ)) > 0
            && spin_count < SPIN_LIMIT
        {
            spin_count += 1;
        }

        if spin_count == SPIN_LIMIT {
            kerror!("Port is hung");
            return Err(SystemError::EIO);
        }

        volatile_set_bit!(port.ci, 1 << slot, true); // Issue command
                                                     // kdebug!("To wait ahci read complete.");
                                                     // 等待操作完成
        loop {
            if (volatile_read!(port.ci) & (1 << slot)) == 0 {
                break;
            }
            if (volatile_read!(port.is) & HBA_PxIS_TFES) > 0 {
                kerror!("Read disk error");
                return Err(SystemError::EIO);
            }
        }

        if kbuf.is_some() {
            buf.copy_from_slice(kbuf.as_ref().unwrap());
        }

        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        // successfully read
        return Ok(count * 512);
    }

    fn write_at(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        assert!((buf.len() & 511) == 0);
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        let check_length = ((count - 1) >> 4) + 1; // prdt length
        if count * 512 > buf.len() || check_length > 8 as usize {
            // 不可能的操作
            return Err(SystemError::E2BIG);
        } else if count == 0 {
            return Ok(0);
        }

        let port = _port(self.ctrl_num, self.port_num);

        volatile_write!(port.is, u32::MAX); // Clear pending interrupt bits

        let slot = port.find_cmdslot().unwrap_or(u32::MAX);

        if slot == u32::MAX {
            return Err(SystemError::EIO);
        }

        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        #[allow(unused_unsafe)]
        let cmdheader: &mut HbaCmdHeader = unsafe {
            (phys_2_virt(
                volatile_read!(port.clb) as usize
                    + slot as usize * size_of::<HbaCmdHeader>() as usize,
            ) as *mut HbaCmdHeader)
                .as_mut()
                .unwrap()
        };
        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        volatile_write_bit!(
            cmdheader.cfl,
            (1 << 5) - 1 as u8,
            (size_of::<FisRegH2D>() / size_of::<u32>()) as u8
        ); // Command FIS size

        volatile_set_bit!(cmdheader.cfl, 7 << 5, true); // (p,c,w)都设置为1, Read/Write bit :  Write from device
        volatile_write!(cmdheader.prdtl, check_length as u16); // PRDT entries count

        // 设置数据存放地址
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        let mut buf_ptr = buf as *const [u8] as *mut usize as usize;

        // 由于目前的内存管理机制无法把用户空间的内存地址转换为物理地址，所以只能先把数据拷贝到内核空间
        // TODO：在内存管理重构后，可以直接使用用户空间的内存地址
        let user_buf = verify_area(VirtAddr::new(buf_ptr as usize), buf.len()).is_ok();
        let mut kbuf = if user_buf {
            let mut x: Vec<u8> = Vec::with_capacity(buf.len());
            x.resize(buf.len(), 0);
            x.copy_from_slice(buf);
            Some(x)
        } else {
            None
        };

        if kbuf.is_some() {
            buf_ptr = kbuf.as_mut().unwrap().as_mut_ptr() as usize;
        }

        #[allow(unused_unsafe)]
        let cmdtbl = unsafe {
            (phys_2_virt(volatile_read!(cmdheader.ctba) as usize) as *mut HbaCmdTable)
                .as_mut()
                .unwrap()
        };
        let mut tmp_count = count;
        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        unsafe {
            // 清空整个table的旧数据
            write_bytes(cmdtbl, 0, 1);
        }

        // 8K bytes (16 sectors) per PRDT
        for i in 0..((volatile_read!(cmdheader.prdtl) - 1) as usize) {
            volatile_write!(cmdtbl.prdt_entry[i].dba, virt_2_phys(buf_ptr) as u64);
            volatile_write_bit!(cmdtbl.prdt_entry[i].dbc, (1 << 22) - 1, 8 * 1024 - 1); // 数据长度
            volatile_set_bit!(cmdtbl.prdt_entry[i].dbc, 1 << 31, true); // 允许中断
            buf_ptr += 8 * 1024;
            tmp_count -= 16;
        }

        // Last entry
        let las = (volatile_read!(cmdheader.prdtl) - 1) as usize;
        volatile_write!(cmdtbl.prdt_entry[las].dba, virt_2_phys(buf_ptr) as u64);
        volatile_set_bit!(cmdtbl.prdt_entry[las].dbc, 1 << 31, true); // 允许中断
        volatile_write_bit!(
            cmdtbl.prdt_entry[las].dbc,
            (1 << 22) - 1,
            ((tmp_count << 9) - 1) as u32
        ); // 数据长度

        // 设置命令
        let cmdfis = unsafe {
            ((&mut cmdtbl.cfis) as *mut [u8] as *mut usize as *mut FisRegH2D)
                .as_mut()
                .unwrap()
        };
        volatile_write!(cmdfis.fis_type, FisType::RegH2D as u8);
        volatile_set_bit!(cmdfis.pm, 1 << 7, true); // command_bit set
        volatile_write!(cmdfis.command, ATA_CMD_WRITE_DMA_EXT);

        volatile_write!(cmdfis.lba0, (lba_id_start & 0xFF) as u8);
        volatile_write!(cmdfis.lba1, ((lba_id_start >> 8) & 0xFF) as u8);
        volatile_write!(cmdfis.lba2, ((lba_id_start >> 16) & 0xFF) as u8);
        volatile_write!(cmdfis.lba3, ((lba_id_start >> 24) & 0xFF) as u8);
        volatile_write!(cmdfis.lba4, ((lba_id_start >> 32) & 0xFF) as u8);
        volatile_write!(cmdfis.lba5, ((lba_id_start >> 40) & 0xFF) as u8);

        volatile_write!(cmdfis.countl, (count & 0xFF) as u8);
        volatile_write!(cmdfis.counth, ((count >> 8) & 0xFF) as u8);

        volatile_write!(cmdfis.device, 1 << 6); // LBA Mode

        volatile_set_bit!(port.ci, 1 << slot, true); // Issue command

        // 等待操作完成
        loop {
            if (volatile_read!(port.ci) & (1 << slot)) == 0 {
                break;
            }
            if (volatile_read!(port.is) & HBA_PxIS_TFES) > 0 {
                kerror!("Write disk error");
                return Err(SystemError::EIO);
            }
        }

        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        // successfully read
        return Ok(count * 512);
    }

    fn sync(&self) -> Result<(), SystemError> {
        // 由于目前没有block cache, 因此sync返回成功即可
        return Ok(());
    }
}

impl LockedAhciDisk {
    pub fn new(
        name: String,
        flags: u16,
        ctrl_num: u8,
        port_num: u8,
    ) -> Result<Arc<LockedAhciDisk>, SystemError> {
        // 构建磁盘结构体
        let result: Arc<LockedAhciDisk> = Arc::new(LockedAhciDisk(SpinLock::new(AhciDisk {
            name,
            flags,
            partitions: Default::default(),
            ctrl_num,
            port_num,
            self_ref: Weak::default(),
        })));

        let table: MbrDiskPartionTable = result.read_mbr_table()?;

        // 求出有多少可用分区
        for i in 0..4 {
            compiler_fence(Ordering::SeqCst);
            if table.dpte[i].part_type != 0 {
                let w = Arc::downgrade(&result);
                result.0.lock().partitions.push(Partition::new(
                    table.dpte[i].starting_sector() as u64,
                    table.dpte[i].starting_lba as u64,
                    table.dpte[i].total_sectors as u64,
                    w,
                    i as u16,
                ));
            }
        }

        result.0.lock().self_ref = Arc::downgrade(&result);

        return Ok(result);
    }

    /// @brief: 从磁盘中读取 MBR 分区表结构体 TODO: Cursor
    pub fn read_mbr_table(&self) -> Result<MbrDiskPartionTable, SystemError> {
        let mut table: MbrDiskPartionTable = Default::default();

        // 数据缓冲区
        let mut buf: Vec<u8> = Vec::new();
        buf.resize(size_of::<MbrDiskPartionTable>(), 0);

        self.read_at(0, 1, &mut buf)?;
        // 创建 Cursor 用于按字节读取
        let mut cursor = VecCursor::new(buf);
        cursor.seek(SeekFrom::SeekCurrent(446))?;

        for i in 0..4 {
            kdebug!("infomation of partition {}:\n", i);

            table.dpte[i].flags = cursor.read_u8()?;
            table.dpte[i].starting_head = cursor.read_u8()?;
            table.dpte[i].starting_sector_cylinder = cursor.read_u16()?;
            table.dpte[i].part_type = cursor.read_u8()?;
            table.dpte[i].ending_head = cursor.read_u8()?;
            table.dpte[i].ending_sector_cylingder = cursor.read_u16()?;
            table.dpte[i].starting_lba = cursor.read_u32()?;
            table.dpte[i].total_sectors = cursor.read_u32()?;

            kdebug!("dpte[i] = {:?}", table.dpte[i]);
        }
        table.bs_trailsig = cursor.read_u16()?;
        // kdebug!("bs_trailsig = {}", unsafe {
        //     read_unaligned(addr_of!(table.bs_trailsig))
        // });

        return Ok(table);
    }
}

impl KObject for LockedAhciDisk {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        todo!()
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        todo!()
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        todo!()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        todo!()
    }

    fn set_inode(&self, _inode: Option<Arc<KernFSInode>>) {
        todo!()
    }

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        todo!()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        todo!()
    }

    fn set_kobj_state(&self, _state: KObjectState) {
        todo!()
    }

    fn name(&self) -> alloc::string::String {
        todo!()
    }

    fn set_name(&self, _name: alloc::string::String) {
        todo!()
    }

    fn set_kset(&self, _kset: Option<Arc<KSet>>) {
        todo!()
    }

    fn set_parent(&self, _parent: Option<Weak<dyn KObject>>) {
        todo!()
    }

    fn set_kobj_type(&self, _ktype: Option<&'static dyn KObjType>) {
        todo!()
    }
}

impl Device for LockedAhciDisk {
    fn dev_type(&self) -> DeviceType {
        return DeviceType::Block;
    }

    fn id_table(&self) -> IdTable {
        todo!()
    }

    fn bus(&self) -> Option<Arc<dyn Bus>> {
        todo!("LockedAhciDisk::bus()")
    }

    fn set_bus(&self, _bus: Option<Arc<dyn Bus>>) {
        todo!("LockedAhciDisk::set_bus()")
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        todo!("LockedAhciDisk::driver()")
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn set_driver(&self, _driver: Option<Weak<dyn Driver>>) {
        todo!("LockedAhciDisk::set_driver()")
    }

    fn can_match(&self) -> bool {
        todo!()
    }

    fn set_can_match(&self, _can_match: bool) {
        todo!()
    }

    fn state_synced(&self) -> bool {
        todo!()
    }
}

impl BlockDevice for LockedAhciDisk {
    #[inline]
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    #[inline]
    fn blk_size_log2(&self) -> u8 {
        9
    }

    fn sync(&self) -> Result<(), SystemError> {
        return self.0.lock().sync();
    }

    #[inline]
    fn device(&self) -> Arc<dyn Device> {
        return self.0.lock().self_ref.upgrade().unwrap();
    }

    fn block_size(&self) -> usize {
        todo!()
    }

    fn partitions(&self) -> Vec<Arc<Partition>> {
        return self.0.lock().partitions.clone();
    }

    #[inline]
    fn read_at(
        &self,
        lba_id_start: BlockId, // 起始lba编号
        count: usize,          // 读取lba的数量
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        self.0.lock().read_at(lba_id_start, count, buf)
    }

    #[inline]
    fn write_at(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        self.0.lock().write_at(lba_id_start, count, buf)
    }
}
