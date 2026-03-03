use core::{
    convert::TryFrom,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU32, Ordering},
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
        vfs::{utils::DName, IndexNode, InodeMode, Metadata},
    },
    libs::{mutex::MutexGuard, rwlock::RwLock},
};

const MINORS_PER_DISK: u32 = 256;

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

impl GenDisk {
    /// 如果gendisk是整个磁盘，则idx为u32::MAX
    pub const ENTIRE_DISK_IDX: u32 = u32::MAX;

    pub fn new(
        bdev: Weak<dyn BlockDevice>,
        range: GeneralBlockRange,
        idx: Option<u32>,
        dev_name: DName,
    ) -> Arc<Self> {
        let bsizelog2 = bdev.upgrade().unwrap().blk_size_log2();

        // 对应整块硬盘的情况
        let id = idx.unwrap_or(0);
        if id >= MINORS_PER_DISK {
            panic!("GenDisk index out of range: {}", id);
        }
        let ptr = bdev.upgrade().unwrap();
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
                InodeMode::from_bits_truncate(0o755),
            ),
            name: dev_name,
        });
    }

    pub fn block_device(&self) -> Arc<dyn BlockDevice> {
        return self.bdev.upgrade().unwrap();
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

        return self.block_device().read_at(lba, blocks, buf);
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
            .block_device()
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
            .block_device()
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
        return self.block_device().write_at(lba, blocks, buf);
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
    pub fn range(&self) -> &GeneralBlockRange {
        &self.range
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
        self.block_device().sync()
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
    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        self.fs.read().upgrade().unwrap()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if len == 0 {
            return Ok(0);
        }
        if len > buf.len() {
            return Err(SystemError::ENOBUFS);
        }
        self.read_at_bytes(&mut buf[..len], offset)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if len == 0 {
            return Ok(0);
        }
        if len > buf.len() {
            return Err(SystemError::E2BIG);
        }
        self.write_at_bytes(&buf[..len], offset)
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, system_error::SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        let mut meta = self.metadata.clone();
        let bdev = self.block_device();
        let range = bdev.disk_range();
        let blocks = range.lba_end.saturating_sub(range.lba_start);
        let size_in_bytes = blocks.saturating_mul(LBA_SIZE);

        meta.size = i64::try_from(size_in_bytes).unwrap_or(i64::MAX);
        meta.blocks = blocks;
        meta.blk_size = LBA_SIZE;
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
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn open(
        &self,
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
        _mode: &crate::filesystem::vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        private_data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let bdev = self.block_device();
        if let Some(loop_dev) = BlockDevice::as_any_ref(&*bdev).downcast_ref::<LoopDevice>() {
            loop_dev.ioctl(cmd, data, private_data)
        } else {
            Err(SystemError::ENOSYS)
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
    max_idx: AtomicU32,
}

impl GenDiskMap {
    pub fn new() -> Self {
        GenDiskMap {
            data: HashMap::new(),
            max_idx: AtomicU32::new(1),
        }
    }

    #[inline]
    #[allow(dead_code)]
    pub fn max_idx(&self) -> u32 {
        self.max_idx.load(Ordering::SeqCst)
    }

    #[inline]
    pub fn alloc_idx(&self) -> u32 {
        self.max_idx.fetch_add(1, Ordering::SeqCst)
    }

    pub fn intersects(&self, range: &GeneralBlockRange) -> bool {
        for (_, v) in self.iter() {
            if range.intersects_with(&v.range).is_some() {
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
