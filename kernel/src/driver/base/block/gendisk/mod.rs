use core::{
    convert::TryFrom,
    fmt::{Debug, Formatter},
    ops::{Deref, DerefMut},
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
};

const MINORS_PER_DISK: u32 = 256;

mod ioctl;

#[derive(Debug)]
pub struct GenDisk {
    bdev: Weak<dyn BlockDevice>,
    range: GeneralBlockRange,
    block_size_log2: u8,
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

        return Arc::new(GenDisk {
            bdev,
            range,
            block_size_log2: bsizelog2,
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

    pub fn acquire_mount_holder(&self) -> Result<GenDiskMountGuard, SystemError> {
        let bdev = self.block_device()?;
        bdev.mount_holder_acquire()?;
        Ok(GenDiskMountGuard { bdev })
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

        let blocks = buf.len() / (1 << self.block_size_log2 as usize);
        let lba = self.block_offset_2_disk_blkid(start_block_offset);

        return self.block_device()?.read_at(lba, blocks, buf);
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
        let start_lba = self.range.lba_start;
        let bytes_offset = self.disk_blkid_2_bytes(start_lba) + bytes_offset;
        return self
            .block_device()?
            .read_at_bytes(bytes_offset, buf.len(), buf);
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
        let start_lba = self.range.lba_start;
        let bytes_offset = self.disk_blkid_2_bytes(start_lba) + bytes_offset;
        return self
            .block_device()?
            .write_at_bytes(bytes_offset, buf.len(), buf);
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

        let blocks = buf.len() / (1 << self.block_size_log2 as usize);
        let lba = self.block_offset_2_disk_blkid(start_block_offset);
        return self.block_device()?.write_at(lba, blocks, buf);
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
        self.block_device()?.sync()
    }

    pub fn symlink_name(&self) -> String {
        let major = self.device_num.major().data();
        let minor = self.device_num.minor();
        format!("{}:{}", major, minor)
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
        if len == 0 {
            return Ok(crate::filesystem::vfs::PrimaryWriteResult::new(0));
        }
        intent
            .io
            .check_alignment(offset | len, 1usize << self.block_size_log2)?;
        stage.complete();
        source.with_buffer(len, |bytes| {
            let count = if matches!(
                intent.io,
                crate::filesystem::vfs::WriteIoIntent::Direct { .. }
            ) {
                bytes.len().min(len) & !((1usize << self.block_size_log2) - 1)
            } else {
                bytes.len().min(len)
            };
            if count == 0 {
                return Err(SystemError::EFAULT.into());
            }
            IndexNode::write_at(self, offset, count, &bytes[..count], data)
                .map(crate::filesystem::vfs::PrimaryWriteResult::new)
        })
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
        if len == 0 {
            return Ok(DelegatedWriteResult::new(0, Ok(())));
        }
        intent
            .io
            .check_alignment(offset | len, 1usize << self.block_size_log2)?;
        stage.complete();
        source.with_buffer(len, |bytes| {
            let count = if matches!(
                intent.io,
                crate::filesystem::vfs::WriteIoIntent::Direct { .. }
            ) {
                bytes.len().min(len) & !((1usize << self.block_size_log2) - 1)
            } else {
                bytes.len().min(len)
            };
            if count == 0 {
                return Err(SystemError::EFAULT.into());
            }
            self.write_at_with_sync(offset, count, &bytes[..count], sync, data)
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
        if self
            .block_device()
            .is_ok_and(|bdev| bdev.uses_delegated_write_sync())
        {
            behavior.post_write_sync = PostWriteSyncPolicy::Delegated;
        }
    }

    fn sync_file(
        &self,
        _datasync: bool,
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let device = self.block_device()?;
        if let Some(loop_device) = BlockDevice::as_any_ref(&*device).downcast_ref::<LoopDevice>() {
            return loop_device.try_sync();
        }
        device.sync().map_err(Into::into)
    }

    fn user_read_mode_with_io_intent(
        &self,
        intent: crate::filesystem::vfs::ReadIoIntent,
    ) -> crate::filesystem::vfs::UserReadMode {
        if intent == crate::filesystem::vfs::ReadIoIntent::Direct {
            crate::filesystem::vfs::UserReadMode::Iterator
        } else {
            self.user_read_mode()
        }
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
        if intent != ReadIoIntent::Direct {
            return self.read_user_at(offset, len, writer, data);
        }
        drop(data);
        let capacity = self
            .range()
            .len()
            .checked_mul(LBA_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        if offset >= capacity {
            return Ok(Some(0));
        }
        let len = len.min(writer.len()).min(capacity - offset);
        if len == 0 {
            return Ok(Some(0));
        }
        let mask = (1usize << self.block_size_log2) - 1;
        // Current block devices expose sector alignment, not a separate DMA
        // alignment property. Keep address/length mask responsibilities clear.
        if offset & mask != 0 || !writer.is_aligned_at(0, len, mask, mask) {
            return Err(SystemError::EINVAL);
        }
        let mut scratch = crate::filesystem::vfs::read_buffer::ReadScratch::default();
        let mut total = 0;
        while total < len {
            let want = (len - total).min(crate::filesystem::vfs::read_buffer::MAX_CHUNK);
            let buffer = scratch.prepare(want)?;
            // Only a not-yet-copied backing attempt participates in typed
            // allocation recovery. Never replay the whole user operation.
            let read = File::recover_io(|| {
                IndexNode::read_at(
                    self,
                    offset + total,
                    want,
                    buffer,
                    Mutex::new(FilePrivateData::Unused).lock(),
                )
            })?;
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

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        if len == 0 {
            return Ok(0);
        }
        if len > buf.len() {
            return Err(SystemError::ENOBUFS.into());
        }
        let disk_offset = self
            .disk_blkid_2_bytes(self.range().lba_start)
            .checked_add(offset)
            .ok_or(SystemError::EOVERFLOW)?;
        let device = self.block_device()?;
        if let Some(loop_device) = BlockDevice::as_any_ref(&*device).downcast_ref::<LoopDevice>() {
            return loop_device.try_read_bytes_operation(disk_offset, len, &mut buf[..len]);
        }
        device
            .read_at_bytes(disk_offset, len, &mut buf[..len])
            .map_err(Into::into)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        if len == 0 {
            return Ok(0);
        }
        if len > buf.len() {
            return Err(SystemError::E2BIG.into());
        }
        let disk_offset = self
            .disk_blkid_2_bytes(self.range().lba_start)
            .checked_add(offset)
            .ok_or(SystemError::EOVERFLOW)?;
        let device = self.block_device()?;
        if let Some(loop_device) = BlockDevice::as_any_ref(&*device).downcast_ref::<LoopDevice>() {
            return loop_device
                .try_write_bytes_operation(disk_offset, len, &buf[..len], WriteSyncIntent::None)
                .and_then(|result| {
                    result.sync_result?;
                    Ok(result.written_len)
                });
        }
        device
            .write_at_bytes(disk_offset, len, &buf[..len])
            .map_err(Into::into)
    }

    fn write_at_with_sync(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        sync_intent: WriteSyncIntent,
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<DelegatedWriteResult, crate::mm::allocation::MemoryAllocationError> {
        if len > buf.len() {
            return Err(SystemError::E2BIG.into());
        }
        let disk_offset = self
            .disk_blkid_2_bytes(self.range().lba_start)
            .checked_add(offset)
            .ok_or(SystemError::EOVERFLOW)?;
        let device = self.block_device()?;
        if let Some(loop_device) = BlockDevice::as_any_ref(&*device).downcast_ref::<LoopDevice>() {
            return loop_device.try_write_bytes_operation(
                disk_offset,
                len,
                &buf[..len],
                sync_intent,
            );
        }
        device
            .write_at_bytes_with_sync(disk_offset, len, &buf[..len], sync_intent)
            .map_err(Into::into)
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
        self.block_device()?.file_close(data)
    }

    fn open(
        &self,
        data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
        mode: &crate::filesystem::vfs::file::FileFlags,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> =
            (|| self.block_device()?.file_open(data, mode))();
        result.map_err(Into::into)
    }

    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        private_data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let bdev = self.block_device()?;
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
