use super::{hba::HbaCmdTable, AhciController, AhciIdentify};
use crate::arch::MMArch;
use crate::driver::base::block::block_device::{BlockDevice, BlockId, GeneralBlockRange};
use crate::driver::base::block::disk_info::Partition;
use crate::driver::base::block::manager::BlockDevMeta;
use crate::driver::base::class::Class;
use crate::driver::base::device::bus::Bus;

use crate::driver::base::device::device_number::Major;
use crate::driver::base::device::driver::Driver;
use crate::driver::base::device::{DevName, Device, DeviceCommonData, DeviceType, IdTable};
use crate::driver::base::kobject::{
    KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState,
};
use crate::driver::base::kset::KSet;
use crate::driver::scsi::scsi_manager;
use crate::filesystem::kernfs::KernFSInode;
use crate::filesystem::vfs::{file::FileFlags, FilePrivateData};

use crate::driver::disk::ahci::hba::{
    FisRegH2D, FisType, HbaCmdHeader, ATA_CMD_READ_DMA_EXT, ATA_CMD_WRITE_DMA_EXT,
};
use crate::libs::mutex::{Mutex, MutexGuard};
use crate::libs::rwsem::{RwSemReadGuard, RwSemWriteGuard};
use crate::mm::{dma::DmaDirection, MemoryManagementArch, PhysAddr};
use log::error;
use system_error::SystemError;

use alloc::string::ToString;
use alloc::sync::Weak;
use alloc::{sync::Arc, vec::Vec};

use core::fmt::Debug;
use core::sync::atomic::{compiler_fence, Ordering};
use core::{mem::size_of, ptr::write_bytes};

/// @brief: 只支持MBR分区格式的磁盘结构体
pub struct AhciDisk {
    // 磁盘的状态flags
    pub partitions: Vec<Arc<Partition>>, // 磁盘分区数组
    // port: &'static mut HbaPort,      // 控制硬盘的端口
    device_common: DeviceCommonData,
    kobject_common: KObjectCommonData,
    /// 指向LockAhciDisk的弱引用
    self_ref: Weak<LockedAhciDisk>,
}

/// @brief: 带锁的AhciDisk
#[derive(Debug)]
pub struct LockedAhciDisk {
    blkdev_meta: BlockDevMeta,
    inner: Mutex<AhciDisk>,
    kobj_state: LockedKObjectState,
    controller: Arc<AhciController>,
    port_num: u8,
    capacity_lba: usize,
    flush_command: Option<u8>,
    reliable_flush: bool,
}

impl LockedAhciDisk {
    pub fn inner(&self) -> MutexGuard<'_, AhciDisk> {
        self.inner.lock()
    }

    pub(crate) fn needs_flush(&self) -> bool {
        self.flush_command.is_some()
    }

    pub(crate) fn teardown_flush_command(&self) -> Option<(usize, u8)> {
        self.flush_command
            .map(|command| (self.port_num as usize, command))
    }
}

/// 函数实现
impl Debug for AhciDisk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "AhciDisk")
    }
}

impl LockedAhciDisk {
    fn read_at(
        &self,
        lba_id_start: BlockId, // 起始lba编号
        count: usize,          // 读取lba的数量
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        if count == 0 {
            return Ok(0);
        }
        if buf.len() & 511 != 0 {
            return Err(SystemError::EINVAL);
        }
        compiler_fence(Ordering::SeqCst);
        let check_length = ((count - 1) >> 4) + 1; // prdt length
        if count.checked_mul(512).ok_or(SystemError::EOVERFLOW)? > buf.len()
            || lba_id_start
                .checked_add(count)
                .ok_or(SystemError::EOVERFLOW)?
                > self.capacity_lba
            || check_length > 8_usize
        {
            error!("ahci read: e2big");
            // 不可能的操作
            return Err(SystemError::E2BIG);
        }
        let _port_guard = self.controller.lock_port_for_io(self.port_num as usize)?;

        let port = unsafe { &mut *self.controller.port_ptr(self.port_num as usize) };
        volatile_write!(port.is, u32::MAX); // Clear pending interrupt bits

        let slot = port
            .find_cmdslot(self.controller.command_slots)
            .unwrap_or(u32::MAX);

        if slot == u32::MAX {
            return Err(SystemError::EIO);
        }

        let clb = volatile_read!(port.clb);
        let cmdheader: &mut HbaCmdHeader = unsafe {
            &mut *(MMArch::phys_2_virt(PhysAddr::new(
                clb as usize + slot as usize * size_of::<HbaCmdHeader>(),
            ))
            .ok_or(SystemError::EFAULT)?
            .data() as *mut HbaCmdHeader)
        };

        cmdheader.cfl = (size_of::<FisRegH2D>() / size_of::<u32>()) as u8;
        volatile_write!(cmdheader._pm, 0);
        volatile_write!(cmdheader._prdbc, 0);
        volatile_write!(cmdheader.prdtl, check_length as u16); // PRDT entries count

        // 设置数据存放地址
        let mut dma = self
            .controller
            .allocate_dma(count * 512, DmaDirection::FromDevice)?;
        let mut buf_paddr = dma.paddr();

        let ctba = volatile_read!(cmdheader.ctba);
        let cmdtbl = unsafe {
            &mut *(MMArch::phys_2_virt(PhysAddr::new(ctba as usize))
                .ok_or(SystemError::EFAULT)?
                .data() as *mut HbaCmdTable)
        };
        let mut tmp_count = count;

        unsafe {
            // 清空整个table的旧数据
            write_bytes(cmdtbl, 0, 1);
        }
        // debug!("cmdheader.prdtl={}", volatile_read!(cmdheader.prdtl));

        // 8K bytes (16 sectors) per PRDT
        for i in 0..((volatile_read!(cmdheader.prdtl) - 1) as usize) {
            volatile_write!(cmdtbl.prdt_entry[i].dba, buf_paddr as u64);
            cmdtbl.prdt_entry[i].dbc = 8 * 1024 - 1;
            volatile_set_bit!(cmdtbl.prdt_entry[i].dbc, 1 << 31, true); // 允许中断 prdt_entry.i
            buf_paddr += 8 * 1024;
            tmp_count -= 16;
        }

        // Last entry
        let las = (volatile_read!(cmdheader.prdtl) - 1) as usize;
        volatile_write!(cmdtbl.prdt_entry[las].dba, buf_paddr as u64);
        cmdtbl.prdt_entry[las].dbc = ((tmp_count << 9) - 1) as u32; // 数据长度

        volatile_set_bit!(cmdtbl.prdt_entry[las].dbc, 1 << 31, true); // 允许中断

        // 设置命令
        let cmdfis = unsafe { &mut *(cmdtbl.cfis.as_mut_ptr() as *mut FisRegH2D) };
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

        AhciController::wait_tfd_ready(port)?;

        compiler_fence(Ordering::Release);
        volatile_set_bit!(port.ci, 1 << slot, true); // Issue command
                                                     // debug!("To wait ahci read complete.");
                                                     // 等待操作完成
        if let Err(err) = AhciController::wait_slot(port, slot) {
            self.controller
                .recover_or_fail_command(self.port_num as usize, Some(dma));
            return Err(err);
        }
        compiler_fence(Ordering::Acquire);
        buf[..count * 512].copy_from_slice(&dma.as_mut_slice()[..count * 512]);

        compiler_fence(Ordering::SeqCst);
        // successfully read
        return Ok(count * 512);
    }

    fn write_at(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        if count == 0 {
            return Ok(0);
        }
        if buf.len() & 511 != 0 {
            return Err(SystemError::EINVAL);
        }
        compiler_fence(Ordering::SeqCst);
        let check_length = ((count - 1) >> 4) + 1; // prdt length
        if count.checked_mul(512).ok_or(SystemError::EOVERFLOW)? > buf.len()
            || lba_id_start
                .checked_add(count)
                .ok_or(SystemError::EOVERFLOW)?
                > self.capacity_lba
            || check_length > 8
        {
            // 不可能的操作
            return Err(SystemError::E2BIG);
        }
        let _port_guard = self.controller.lock_port_for_io(self.port_num as usize)?;

        let port = unsafe { &mut *self.controller.port_ptr(self.port_num as usize) };

        volatile_write!(port.is, u32::MAX); // Clear pending interrupt bits

        let slot = port
            .find_cmdslot(self.controller.command_slots)
            .unwrap_or(u32::MAX);

        if slot == u32::MAX {
            return Err(SystemError::EIO);
        }

        compiler_fence(Ordering::SeqCst);
        let clb = volatile_read!(port.clb);
        let cmdheader: &mut HbaCmdHeader = unsafe {
            &mut *(MMArch::phys_2_virt(PhysAddr::new(
                clb as usize + slot as usize * size_of::<HbaCmdHeader>(),
            ))
            .ok_or(SystemError::EFAULT)?
            .data() as *mut HbaCmdHeader)
        };
        compiler_fence(Ordering::SeqCst);

        // DW0: CFL in bits 0..4, W in bit 6. This is an ATA write, not ATAPI.
        volatile_write!(
            cmdheader.cfl,
            (size_of::<FisRegH2D>() / size_of::<u32>()) as u8 | (1 << 6)
        );
        volatile_write!(cmdheader._pm, 0);
        volatile_write!(cmdheader._prdbc, 0);
        volatile_write!(cmdheader.prdtl, check_length as u16); // PRDT entries count

        // 设置数据存放地址
        compiler_fence(Ordering::SeqCst);
        let mut dma = self
            .controller
            .allocate_dma(count * 512, DmaDirection::ToDevice)?;
        dma.as_mut_slice()[..count * 512].copy_from_slice(&buf[..count * 512]);
        let mut buf_paddr = dma.paddr();

        let ctba = volatile_read!(cmdheader.ctba);
        let cmdtbl = unsafe {
            &mut *(MMArch::phys_2_virt(PhysAddr::new(ctba as usize))
                .ok_or(SystemError::EFAULT)?
                .data() as *mut HbaCmdTable)
        };
        let mut tmp_count = count;
        compiler_fence(Ordering::SeqCst);

        unsafe {
            // 清空整个table的旧数据
            write_bytes(cmdtbl, 0, 1);
        }

        // 8K bytes (16 sectors) per PRDT
        for i in 0..((volatile_read!(cmdheader.prdtl) - 1) as usize) {
            volatile_write!(cmdtbl.prdt_entry[i].dba, buf_paddr as u64);
            volatile_write_bit!(cmdtbl.prdt_entry[i].dbc, (1 << 22) - 1, 8 * 1024 - 1); // 数据长度
            volatile_set_bit!(cmdtbl.prdt_entry[i].dbc, 1 << 31, true); // 允许中断
            buf_paddr += 8 * 1024;
            tmp_count -= 16;
        }

        // Last entry
        let las = (volatile_read!(cmdheader.prdtl) - 1) as usize;
        volatile_write!(cmdtbl.prdt_entry[las].dba, buf_paddr as u64);
        volatile_set_bit!(cmdtbl.prdt_entry[las].dbc, 1 << 31, true); // 允许中断
        volatile_write_bit!(
            cmdtbl.prdt_entry[las].dbc,
            (1 << 22) - 1,
            ((tmp_count << 9) - 1) as u32
        ); // 数据长度

        // 设置命令
        let cmdfis = unsafe { &mut *(cmdtbl.cfis.as_mut_ptr() as *mut FisRegH2D) };
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

        AhciController::wait_tfd_ready(port)?;
        compiler_fence(Ordering::Release);
        volatile_set_bit!(port.ci, 1 << slot, true); // Issue command

        // 等待操作完成
        if let Err(err) = AhciController::wait_slot(port, slot) {
            self.controller
                .recover_or_fail_command(self.port_num as usize, Some(dma));
            return Err(err);
        }
        compiler_fence(Ordering::Acquire);

        compiler_fence(Ordering::SeqCst);
        // successfully read
        return Ok(count * 512);
    }

    fn sync_disk(&self) -> Result<(), SystemError> {
        // If IDENTIFY selected no flush command, completed DMA writes are still
        // successful; only the power-loss durability guarantee is unavailable,
        // which `supports_reliable_flush()` reports separately to filesystems.
        let Some(command) = self.flush_command else {
            return Ok(());
        };
        self.controller.flush_port(self.port_num as usize, command)
    }
}

impl LockedAhciDisk {
    pub fn new(
        controller: Arc<AhciController>,
        port_num: u8,
        identify: AhciIdentify,
    ) -> Result<Arc<LockedAhciDisk>, SystemError> {
        let devname = scsi_manager().alloc_id().ok_or(SystemError::EBUSY)?;
        let parent_device: Arc<dyn Device> = controller.device.clone();
        let parent_kobject: Arc<dyn KObject> = controller.device.clone();
        // 构建磁盘结构体
        let result: Arc<LockedAhciDisk> = Arc::new_cyclic(|self_ref| LockedAhciDisk {
            blkdev_meta: BlockDevMeta::new(devname, Major::AHCI_BLK_MAJOR),
            inner: Mutex::new(AhciDisk {
                partitions: Vec::new(),
                device_common: DeviceCommonData {
                    parent: Some(Arc::downgrade(&parent_device)),
                    ..Default::default()
                },
                kobject_common: KObjectCommonData {
                    parent: Some(Arc::downgrade(&parent_kobject)),
                    ..Default::default()
                },
                self_ref: self_ref.clone(),
            }),
            kobj_state: LockedKObjectState::default(),
            controller,
            port_num,
            capacity_lba: identify.capacity_lba,
            flush_command: identify.flush_command,
            reliable_flush: identify.reliable_flush,
        });
        return Ok(result);
    }
}

impl Drop for LockedAhciDisk {
    fn drop(&mut self) {
        scsi_manager().free_id(self.blkdev_meta.devname.id());
    }
}

impl KObject for LockedAhciDisk {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kobject_common.kern_inode.clone()
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner().kobject_common.kobj_type
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner().kobject_common.kset.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner().kobject_common.parent.clone()
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kobject_common.kern_inode = inode;
    }

    fn kobj_state(&self) -> RwSemReadGuard<'_, KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwSemWriteGuard<'_, KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state.write() = state;
    }

    fn name(&self) -> alloc::string::String {
        self.dev_name().to_string()
    }

    fn set_name(&self, _name: alloc::string::String) {}

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner().kobject_common.kset = kset;
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner().kobject_common.parent = parent;
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobject_common.kobj_type = ktype;
    }
}

impl Device for LockedAhciDisk {
    fn dev_type(&self) -> DeviceType {
        return DeviceType::Block;
    }

    fn id_table(&self) -> IdTable {
        IdTable::new("ahci".to_string(), None)
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner().device_common.bus.clone()
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner().device_common.bus = bus;
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        let mut inner = self.inner();
        let driver = inner.device_common.driver.clone()?.upgrade();
        if driver.is_none() {
            inner.device_common.driver = None;
        }
        driver
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.inner().device_common.driver = driver;
    }

    fn can_match(&self) -> bool {
        self.inner().device_common.can_match
    }

    fn set_can_match(&self, can_match: bool) {
        self.inner().device_common.can_match = can_match;
    }

    fn state_synced(&self) -> bool {
        true
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        let mut inner = self.inner();
        let class = inner.device_common.class.clone()?.upgrade();
        if class.is_none() {
            inner.device_common.class = None;
        }
        class
    }

    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.inner().device_common.class = class;
    }

    fn dev_parent(&self) -> Option<Weak<dyn Device>> {
        self.inner().device_common.get_parent_weak_or_clear()
    }

    fn set_dev_parent(&self, dev_parent: Option<Weak<dyn Device>>) {
        self.inner().device_common.parent = dev_parent;
    }
}

impl BlockDevice for LockedAhciDisk {
    fn file_open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        self.controller.acquire_holder()
    }

    fn mount_holder_acquire(&self) -> Result<(), SystemError> {
        self.controller.acquire_holder()
    }

    fn dev_name(&self) -> &DevName {
        &self.blkdev_meta.devname
    }

    fn blkdev_meta(&self) -> &BlockDevMeta {
        &self.blkdev_meta
    }

    fn disk_range(&self) -> GeneralBlockRange {
        GeneralBlockRange::new(0, self.capacity_lba)
            .expect("IDENTIFY returned a non-zero AHCI capacity")
    }

    #[inline]
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    #[inline]
    fn blk_size_log2(&self) -> u8 {
        9
    }

    fn sync(&self) -> Result<(), SystemError> {
        self.sync_disk()
    }

    fn supports_reliable_flush(&self) -> bool {
        self.reliable_flush
    }

    #[inline]
    fn device(&self) -> Arc<dyn Device> {
        return self.inner().self_ref.upgrade().unwrap();
    }

    fn block_size(&self) -> usize {
        512
    }

    fn partitions(&self) -> Vec<Arc<Partition>> {
        return self.inner().partitions.clone();
    }

    #[inline]
    fn read_at_sync(
        &self,
        lba_id_start: BlockId, // 起始lba编号
        count: usize,          // 读取lba的数量
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        self.read_at(lba_id_start, count, buf)
    }

    #[inline]
    fn write_at_sync(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        self.write_at(lba_id_start, count, buf)
    }
}
