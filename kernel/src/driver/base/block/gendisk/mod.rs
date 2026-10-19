use core::{
    convert::TryFrom,
    fmt::{Debug, Formatter},
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

use alloc::{
    string::String,
    sync::{Arc, Weak},
};
use hashbrown::HashMap;
use system_error::SystemError;

use super::block_device::{BlockDevice, BlockId, GeneralBlockRange, LBA_SIZE};
use crate::{
    driver::{base::device::device_number::DeviceNumber, block::loop_device::LoopDevice},
    filesystem::{
        devfs::{DevFS, DeviceINode, LockedDevFSInode},
        vfs::{
            utils::DName, DelegatedWriteResult, FilePrivateData, IndexNode, Metadata,
            OpenFileBehavior, PostWriteSyncPolicy, WriteSyncIntent,
        },
    },
    libs::{
        mutex::{Mutex, MutexGuard},
        rwlock::RwLock,
    },
    mm::MemoryManagementArch,
};

const MINORS_PER_DISK: u32 = 256;

mod cache;
mod ioctl;
mod lifecycle;
#[cfg(feature = "mm_selftest")]
pub(crate) use cache::run_raw_block_prefix_selftest;

#[derive(Debug)]
pub struct GenDisk {
    bdev: Weak<dyn BlockDevice>,
    self_ref: Weak<GenDisk>,
    raw_cache: Mutex<Option<Arc<crate::filesystem::page_cache::PageCache>>>,
    online: AtomicBool,
    open_holders: AtomicUsize,
    mount_holders: AtomicUsize,
    mount_claimant: AtomicU64,
    range: GeneralBlockRange,
    block_size_log2: u8,
    soft_block_size: AtomicUsize,
    idx: Option<u32>,

    device_num: DeviceNumber,

    parent: RwLock<Weak<LockedDevFSInode>>,
    fs: RwLock<Weak<DevFS>>,
    metadata: Metadata,
    /// 对应/dev/下的设备名
    name: DName,
}

pub struct GenDiskMountGuard {
    bdev: Arc<dyn BlockDevice>,
    disk: Arc<GenDisk>,
    claimant: crate::filesystem::vfs::Magic,
}

impl Debug for GenDiskMountGuard {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GenDiskMountGuard")
            .field("device", self.bdev.dev_name())
            .finish()
    }
}

impl Drop for GenDiskMountGuard {
    fn drop(&mut self) {
        {
            let _gate = self.bdev.blkdev_meta().raw_cache_gate.write();
            self.disk.release_holder_locked(&*self.bdev, true);
        }
        self.bdev.mount_holder_release();
    }
}

impl GenDisk {
    /// 如果gendisk是整个磁盘，则idx为u32::MAX
    pub const ENTIRE_DISK_IDX: u32 = u32::MAX;

    pub fn new(
        bdev: Weak<dyn BlockDevice>,
        range: GeneralBlockRange,
        idx: Option<u32>,
        dev_name: DName,
    ) -> Arc<Self> {
        let ptr = bdev
            .upgrade()
            .expect("backing block device disappeared while creating GenDisk");
        let bsizelog2 = ptr.blk_size_log2();

        // 对应整块硬盘的情况
        let id = idx.unwrap_or(0);
        if id >= MINORS_PER_DISK {
            panic!("GenDisk index out of range: {}", id);
        }
        let meta = ptr.blkdev_meta();
        let major = meta.major;

        let minor = meta.base_minor * MINORS_PER_DISK + id;
        // log::info!("New gendisk: major: {}, minor: {}", major, minor);
        let device_num = DeviceNumber::new(major, minor);

        return Arc::new_cyclic(|weak| GenDisk {
            bdev,
            self_ref: weak.clone(),
            raw_cache: Mutex::new(None),
            online: AtomicBool::new(true),
            open_holders: AtomicUsize::new(0),
            mount_holders: AtomicUsize::new(0),
            mount_claimant: AtomicU64::new(0),
            range,
            block_size_log2: bsizelog2,
            soft_block_size: AtomicUsize::new(crate::arch::MMArch::PAGE_SIZE),
            idx,
            device_num,
            parent: RwLock::new(Weak::default()),
            fs: RwLock::new(Weak::default()),
            metadata: Metadata::new(
                crate::filesystem::vfs::FileType::BlockDevice,
                ptr.devfs_mode(),
            ),
            name: dev_name,
        });
    }

    pub fn block_device(&self) -> Result<Arc<dyn BlockDevice>, SystemError> {
        self.bdev.upgrade().ok_or(SystemError::ENODEV)
    }

    pub fn acquire_mount_holder(
        &self,
        claimant: crate::filesystem::vfs::Magic,
    ) -> Result<GenDiskMountGuard, SystemError> {
        let bdev = self.block_device()?;
        let _gate = bdev.blkdev_meta().raw_cache_gate.write();
        self.check_online()?;
        self.check_mount_topology_locked(&*bdev)?;
        if claimant.is_empty() {
            return Err(SystemError::EINVAL);
        }
        let owner = self.mount_claimant.load(Ordering::Acquire);
        if owner != 0 && owner != claimant.bits() {
            return Err(SystemError::EBUSY);
        }
        self.prepare_holder_locked(&*bdev)?;
        let disk = self.self_ref.upgrade().ok_or(SystemError::ENODEV)?;
        bdev.mount_holder_acquire()?;
        self.mount_claimant
            .store(claimant.bits(), Ordering::Release);
        self.add_holder_locked(&*bdev, true);
        drop(_gate);
        Ok(GenDiskMountGuard {
            bdev,
            disk,
            claimant,
        })
    }

    pub(crate) fn has_node_holders(&self) -> bool {
        self.open_holders.load(Ordering::Acquire) != 0
            || self.mount_holders.load(Ordering::Acquire) != 0
    }

    pub(crate) fn set_online_locked(&self, online: bool) {
        self.online.store(online, Ordering::Release);
    }

    fn check_online(&self) -> Result<(), SystemError> {
        if self.online.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(SystemError::ENXIO)
        }
    }

    fn check_writable(&self, device: &dyn BlockDevice) -> Result<(), SystemError> {
        if device.is_read_only() {
            Err(SystemError::EPERM)
        } else {
            Ok(())
        }
    }

    /// # read_at
    ///
    /// 读取分区内的数据
    ///
    /// ## 参数
    ///
    /// - buf: 输出缓冲区，大小必须为LBA_SIZE的整数倍，否则返回EINVAL
    /// - start_block_offset: 分区内的块号
    pub fn read_at(
        &self,
        buf: &mut [u8],
        start_block_offset: BlockId,
    ) -> Result<usize, SystemError> {
        if (buf.len() & (LBA_SIZE - 1)) > 0 {
            return Err(SystemError::EINVAL);
        }

        let offset = start_block_offset
            .checked_mul(LBA_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        // Filesystem adapters can retain upper FS resources across this
        // legacy boundary. Recovery, as well as the attempt, must stay NOIO.
        let _noio = crate::cgroup::memory::io::MemoryIoScope::enter_noio();
        crate::filesystem::vfs::file::File::recover_io(|| self.coherent_read(offset, buf))
    }

    /// # read_at_bytes
    ///
    /// 按字节偏移量从分区中读取数据
    ///
    /// ## 参数
    ///
    /// - buf: 输出缓冲区
    /// - bytes_offset: 分区内的字节偏移量
    pub fn read_at_bytes(&self, buf: &mut [u8], bytes_offset: usize) -> Result<usize, SystemError> {
        let _noio = crate::cgroup::memory::io::MemoryIoScope::enter_noio();
        crate::filesystem::vfs::file::File::recover_io(|| self.coherent_read(bytes_offset, buf))
    }

    /// # 分区内的字节偏移量转换为磁盘上的字节偏移量
    pub fn disk_bytes_offset(&self, bytes_offset: usize) -> usize {
        let start_lba = self.range.lba_start;
        return self.disk_blkid_2_bytes(start_lba) + bytes_offset;
    }

    /// # write_at_bytes
    ///
    /// 按字节偏移量向分区写入数据
    ///
    /// ## 参数
    ///
    /// - buf: 输入缓冲区
    /// - bytes_offset: 分区内的字节偏移量
    pub fn write_at_bytes(&self, buf: &[u8], bytes_offset: usize) -> Result<usize, SystemError> {
        let _noio = crate::cgroup::memory::io::MemoryIoScope::enter_noio();
        let result = crate::filesystem::vfs::file::File::recover_io(|| {
            self.coherent_write(bytes_offset, buf, WriteSyncIntent::None)
        })?;
        // The lower owner has already submitted this prefix and owns exact
        // sync recovery. Its completion error must never replay the payload.
        result.sync_result.map_err(|error| match error {
            crate::mm::allocation::MemoryAllocationError::System(error) => error,
            crate::mm::allocation::MemoryAllocationError::Charge(_) => SystemError::ENOMEM,
        })?;
        Ok(result.written_len)
    }

    /// # write_at
    ///
    /// 向分区内写入数据
    ///
    /// ## 参数
    ///
    /// - buf: 输入缓冲区，大小必须为LBA_SIZE的整数倍，否则返回EINVAL
    /// - start_block_offset: 分区内的块号
    pub fn write_at(&self, buf: &[u8], start_block_offset: BlockId) -> Result<usize, SystemError> {
        if (buf.len() & (LBA_SIZE - 1)) > 0 {
            return Err(SystemError::EINVAL);
        }

        let offset = start_block_offset
            .checked_mul(LBA_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        self.write_at_bytes(buf, offset)
    }

    #[inline]
    pub fn block_offset_2_disk_blkid(&self, block_offset: BlockId) -> BlockId {
        self.range.lba_start + block_offset
    }

    #[inline]
    fn disk_blkid_2_bytes(&self, disk_blkid: BlockId) -> usize {
        disk_blkid * LBA_SIZE
    }

    #[inline]
    pub fn idx(&self) -> u32 {
        self.idx.unwrap_or(Self::ENTIRE_DISK_IDX)
    }

    #[inline]
    pub fn range(&self) -> GeneralBlockRange {
        // A whole-disk node represents the current capacity of its backing
        // block device.  This is especially important for loop devices: their
        // GenDisk is registered while unbound and gains capacity only after
        // LOOP_SET_FD.  Partitions, in contrast, keep the fixed range parsed
        // from their partition table.
        if self.idx.is_none() {
            self.bdev
                .upgrade()
                .map(|bdev| bdev.disk_range())
                .unwrap_or(self.range)
        } else {
            self.range
        }
    }

    #[inline]
    pub fn device_num(&self) -> DeviceNumber {
        self.device_num
    }

    #[inline]
    pub fn minor(&self) -> u32 {
        self.device_num.minor()
    }

    /// # sync
    /// 同步磁盘
    pub fn sync(&self) -> Result<(), SystemError> {
        let _noio = crate::cgroup::memory::io::MemoryIoScope::enter_noio();
        crate::filesystem::vfs::file::File::recover_io(|| self.sync_owner_range(0, usize::MAX))
    }

    pub fn symlink_name(&self) -> String {
        let major = self.device_num.major().data();
        let minor = self.device_num.minor();
        format!("{}:{}", major, minor)
    }

    fn byte_capacity(&self) -> Result<usize, SystemError> {
        self.range()
            .len()
            .checked_mul(LBA_SIZE)
            .ok_or(SystemError::EOVERFLOW)
    }

    /// Block direct I/O clips against this node before checking alignment.
    /// Kernel callers supply CPU slices; user address admission stays at the
    /// original iterator entry below.
    fn direct_read_length(&self, offset: usize, len: usize) -> Result<usize, SystemError> {
        let capacity = self.byte_capacity()?;
        if offset >= capacity || len == 0 {
            return Ok(0);
        }
        let len = len.min(capacity - offset);
        let mask = self.block_device()?.io_limits().logical_block_size as usize - 1;
        if (offset | len) & mask != 0 {
            return Err(SystemError::EINVAL);
        }
        Ok(len)
    }

    pub fn block_size_log2(&self) -> u8 {
        self.block_size_log2
    }
}

impl IndexNode for GenDisk {
    fn write_primary_with_privilege_stage(
        &self,
        offset: usize,
        len: usize,
        source: crate::filesystem::vfs::PrimaryWriteSource<'_, '_>,
        intent: crate::filesystem::vfs::WriteOperation,
        data: MutexGuard<FilePrivateData>,
        stage: &mut crate::filesystem::vfs::WritePrivilegeStage<'_>,
    ) -> Result<
        crate::filesystem::vfs::PrimaryWriteResult,
        crate::mm::allocation::MemoryAllocationError,
    > {
        let result = self.write_primary_with_sync_with_privilege_stage(
            offset,
            len,
            source,
            intent,
            WriteSyncIntent::None,
            data,
            stage,
        )?;
        result.sync_result?;
        Ok(crate::filesystem::vfs::PrimaryWriteResult::new(
            result.written_len,
        ))
    }

    fn write_primary_with_sync_with_privilege_stage(
        &self,
        offset: usize,
        len: usize,
        source: crate::filesystem::vfs::PrimaryWriteSource<'_, '_>,
        intent: crate::filesystem::vfs::WriteOperation,
        sync: WriteSyncIntent,
        data: MutexGuard<FilePrivateData>,
        stage: &mut crate::filesystem::vfs::WritePrivilegeStage<'_>,
    ) -> Result<DelegatedWriteResult, crate::mm::allocation::MemoryAllocationError> {
        drop(data);
        self.check_writable(&*self.block_device()?)?;
        if len == 0 {
            return Ok(DelegatedWriteResult::new(0, Ok(())));
        }
        let capacity = self.byte_capacity()?;
        if offset >= capacity {
            return Err(SystemError::ENOSPC.into());
        }
        let len = len.min(capacity - offset);
        intent
            .io
            .check_alignment(offset | len, 1usize << self.block_size_log2)?;
        stage.complete();
        source.with_buffer(len, |bytes| {
            let direct = matches!(
                intent.io,
                crate::filesystem::vfs::WriteIoIntent::Direct { .. }
            );
            let count = if direct {
                bytes.len().min(len) & !((1usize << self.block_size_log2) - 1)
            } else {
                bytes.len().min(len)
            };
            if count == 0 {
                return Err(SystemError::EFAULT.into());
            }
            if direct {
                return self.coherent_write_operation(offset, &bytes[..count], sync, Some(len));
            }
            self.buffered_write_operation(offset, &bytes[..count], sync)
        })
    }

    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        self.fs.read().upgrade().unwrap()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn configure_open_file(&self, _data: &FilePrivateData, behavior: &mut OpenFileBehavior) {
        behavior.direct_io = crate::filesystem::vfs::file::DirectIoPolicy::OwnerManaged;
        behavior.post_write_sync = PostWriteSyncPolicy::Delegated;
    }

    fn sync(&self) -> Result<(), SystemError> {
        GenDisk::sync(self)
    }

    fn datasync(&self) -> Result<(), SystemError> {
        GenDisk::sync(self)
    }

    fn sync_file(
        &self,
        _datasync: bool,
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        self.sync_owner_range(0, usize::MAX)
    }

    fn sync_file_range(
        &self,
        start: usize,
        end: usize,
        _datasync: bool,
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        if start > end {
            return Ok(());
        }
        self.sync_owner_range(
            start >> crate::arch::MMArch::PAGE_SHIFT,
            end >> crate::arch::MMArch::PAGE_SHIFT,
        )
    }

    fn user_read_mode_with_io_intent(
        &self,
        intent: crate::filesystem::vfs::ReadIoIntent,
    ) -> crate::filesystem::vfs::UserReadMode {
        let _ = intent;
        crate::filesystem::vfs::UserReadMode::Iterator
    }

    fn read_user_with_io_intent(
        &self,
        offset: usize,
        len: usize,
        writer: &mut crate::syscall::user_buffer::UserBuffer<'_>,
        data: MutexGuard<FilePrivateData>,
        intent: crate::filesystem::vfs::ReadIoIntent,
    ) -> Result<Option<usize>, SystemError> {
        use crate::filesystem::vfs::{file::File, ReadIoIntent};
        drop(data);
        let direct = intent == ReadIoIntent::Direct;
        let len = if direct {
            self.direct_read_length(offset, len.min(writer.len()))?
        } else {
            len.min(writer.len())
                .min(self.byte_capacity()?.saturating_sub(offset))
        };
        if len == 0 {
            return Ok(Some(0));
        }
        // DMA/user-address alignment remains sector-sized even when loop's
        // queue logical block size is larger. Payload admission checks the
        // dynamic logical offset/length under the configuration gate.
        let mask = (1usize << self.block_size_log2) - 1;
        if direct && !writer.is_aligned_at(0, len, mask, mask) {
            return Err(SystemError::EINVAL);
        }
        let mut scratch = crate::filesystem::vfs::read_buffer::ReadScratch::default();
        let mut total = 0;
        while total < len {
            let want = if direct {
                (len - total).min(crate::filesystem::vfs::read_buffer::MAX_CHUNK)
            } else {
                (len - total).min(
                    crate::arch::MMArch::PAGE_SIZE
                        - ((offset + total) & crate::arch::MMArch::PAGE_OFFSET_MASK),
                )
            };
            let buffer = match scratch.prepare(want) {
                Ok(buffer) => buffer,
                Err(_) if total != 0 => return Ok(Some(total)),
                Err(error) => return Err(error),
            };
            // Only a not-yet-copied backing attempt participates in typed
            // allocation recovery. Never replay the whole user operation.
            let result = File::recover_io(|| {
                if direct {
                    self.coherent_read_operation(offset + total, buffer, true)
                } else {
                    self.buffered_read(offset + total, buffer)
                }
            });
            let read = match result {
                Ok(read) => read,
                Err(_) if total != 0 => return Ok(Some(total)),
                Err(error) => return Err(error),
            };
            if read == 0 {
                break;
            }
            let copied = match writer.write_prefix_to_user(total, &buffer[..read]) {
                Ok(copied) => copied,
                Err(SystemError::EFAULT) if total != 0 => return Ok(Some(total)),
                Err(error) => return Err(error),
            };
            total += copied;
            if copied != read {
                // Legacy FAT DIO / a completed raw-device BIO retain the
                // bytes transferred before a later destination-page fault.
                return Ok(Some(total));
            }
        }
        Ok(Some(total))
    }

    fn read_at_with_io_intent(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
        intent: crate::filesystem::vfs::ReadIoIntent,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        if intent != crate::filesystem::vfs::ReadIoIntent::Direct {
            return IndexNode::read_at(self, offset, len, buf, data);
        }
        if len > buf.len() {
            return Err(SystemError::ENOBUFS.into());
        }
        let len = self.direct_read_length(offset, len)?;
        if len == 0 {
            return Ok(0);
        }
        drop(data);
        self.coherent_read_operation(offset, &mut buf[..len], true)
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        if len > buf.len() {
            return Err(SystemError::ENOBUFS.into());
        }
        self.buffered_read(offset, &mut buf[..len])
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        if len > buf.len() {
            return Err(SystemError::E2BIG.into());
        }
        self.buffered_write(offset, &buf[..len])
    }

    fn write_at_with_sync(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        sync: WriteSyncIntent,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<DelegatedWriteResult, crate::mm::allocation::MemoryAllocationError> {
        drop(data);
        if len > buf.len() {
            return Err(SystemError::E2BIG.into());
        }
        self.buffered_write_operation(offset, &buf[..len], sync)
    }

    fn page_cache(&self) -> Option<Arc<crate::filesystem::page_cache::PageCache>> {
        self.existing_raw_cache()
    }

    fn fadvise(
        &self,
        file: &Arc<crate::filesystem::vfs::file::File>,
        offset: i64,
        len: i64,
        advice: i32,
    ) -> Result<usize, SystemError> {
        let device = self.block_device()?;
        let _noio = crate::cgroup::memory::io::MemoryIoScope::enter_noio();
        let _gate = device.blkdev_meta().raw_cache_gate.write();
        self.check_online()?;
        if advice == 3 && self.byte_capacity()? != 0 {
            self.raw_cache();
        }
        crate::mm::syscall::generic_fadvise(file, offset, len, advice, |_, first, last| {
            self.sync_raw_cache_range_locked(first, last)
        })
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, system_error::SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        let mut meta = self.metadata.clone();
        let range = self.range();
        let blocks = range.lba_end.saturating_sub(range.lba_start);
        let size_in_bytes = blocks.saturating_mul(LBA_SIZE);

        meta.size = i64::try_from(size_in_bytes).unwrap_or(i64::MAX);
        meta.blocks = blocks;
        meta.blk_size = LBA_SIZE;
        meta.raw_dev = self.device_num;
        Ok(meta)
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(self.name.clone())
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        let parent = self.parent.read();
        if let Some(parent) = parent.upgrade() {
            return Ok(parent as Arc<dyn IndexNode>);
        }
        Err(SystemError::ENOENT)
    }

    fn close(
        &self,
        data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<(), SystemError> {
        let device = self.block_device()?;
        {
            let _gate = device.blkdev_meta().raw_cache_gate.write();
            self.release_holder_locked(&*device, false);
        }
        // The last lower release can synchronously autoclear and take write.
        device.file_close(data)
    }

    fn open(
        &self,
        data: MutexGuard<FilePrivateData>,
        mode: &crate::filesystem::vfs::file::FileFlags,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let device = self.block_device()?;
        let _gate = device.blkdev_meta().raw_cache_gate.write();
        self.check_online()?;
        self.prepare_holder_locked(&*device)?;
        device.file_open(data, mode)?;
        self.add_holder_locked(&*device, false);
        Ok(())
    }

    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        private_data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let bdev = self.block_device()?;
        if cmd == 0x4008_1271 {
            drop(private_data);
            if !crate::process::cred::capable(crate::process::cred::CAPFlags::CAP_SYS_ADMIN) {
                return Err(SystemError::EACCES.into());
            }
            if data == 0 {
                return Err(SystemError::EINVAL.into());
            }
            let reader = crate::syscall::user_access::UserBufferReader::new(
                data as *const u8,
                core::mem::size_of::<i32>(),
                true,
            )?;
            let size: i32 = reader.buffer_protected(0)?.read_one(0)?;
            self.set_soft_block_size(size as usize)?;
            return Ok(0);
        }
        if cmd == 0x1261 {
            // BLKFLSBUF
            drop(private_data);
            if !crate::process::cred::capable(crate::process::cred::CAPFlags::CAP_SYS_ADMIN) {
                return Err(SystemError::EACCES.into());
            }
            let _noio = crate::cgroup::memory::io::MemoryIoScope::enter_noio();
            let _gate = bdev.blkdev_meta().raw_cache_gate.write();
            self.check_online()?;
            // Linux ignores sync_blockdev errors, but mapping errseq remains.
            let _ = self.sync_raw_cache_locked();
            self.invalidate_raw_cache_locked()?;
            return Ok(0);
        }
        if let Some(query) = ioctl::BlockIoctl::from_cmd(cmd) {
            // Generic queries do not use per-open driver state. In particular,
            // retain no such mutex while protected user copying may fault.
            drop(private_data);
            return query.execute(self, bdev.as_ref(), data).map_err(Into::into);
        }
        if let Some(loop_dev) = BlockDevice::as_any_ref(&*bdev).downcast_ref::<LoopDevice>() {
            loop_dev.ioctl(cmd, data, private_data)
        } else {
            Err(SystemError::ENOTTY.into())
        }
    }
}

impl DeviceINode for GenDisk {
    fn set_fs(&self, fs: alloc::sync::Weak<crate::filesystem::devfs::DevFS>) {
        *self.fs.write() = fs;
    }

    fn set_parent(&self, parent: Weak<LockedDevFSInode>) {
        *self.parent.write() = parent;
    }
}

#[derive(Default)]
pub struct GenDiskMap {
    data: HashMap<u32, Arc<GenDisk>>,
}

impl GenDiskMap {
    pub fn new() -> Self {
        GenDiskMap {
            data: HashMap::new(),
        }
    }

    /// Check overlap between partitions.  The whole-disk entry intentionally
    /// covers every partition and therefore does not participate in this
    /// validation.
    pub fn intersects_partition(&self, range: &GeneralBlockRange) -> bool {
        for (idx, v) in self.iter() {
            if *idx == GenDisk::ENTIRE_DISK_IDX {
                continue;
            }
            if range.intersects_with(&v.range()).is_some() {
                return true;
            }
        }
        return false;
    }
}

impl Deref for GenDiskMap {
    type Target = HashMap<u32, Arc<GenDisk>>;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl DerefMut for GenDiskMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}
