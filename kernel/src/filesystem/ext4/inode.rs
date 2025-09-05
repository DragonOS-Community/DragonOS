use crate::{
    filesystem::{
        page_cache::PageCache,
        vfs::{
            self, syscall::ModeType, utils::DName, vcore::generate_inode_id, FilePrivateData,
            IndexNode, InodeId,
        },
    },
    libs::casting::DowncastArc,
    libs::spinlock::{SpinLock, SpinLockGuard},
    time::PosixTimeSpec,
};
use alloc::{
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::fmt::Debug;
use kdepends::another_ext4::{self, FileType};
use num::ToPrimitive;
use system_error::SystemError;

use super::filesystem::Ext4FileSystem;

type PrivateData<'a> = crate::libs::spinlock::SpinLockGuard<'a, vfs::FilePrivateData>;

pub struct Ext4Inode {
    // 对应another_ext4里面的inode号，用于在ext4文件系统中查找相应的inode
    pub(super) inner_inode_num: u32,
    pub(super) fs_ptr: Weak<super::filesystem::Ext4FileSystem>,
    pub(super) page_cache: Option<Arc<PageCache>>,
    pub(super) children: BTreeMap<DName, Arc<LockedExt4Inode>>,
    pub(super) dname: DName,

    // 对应vfs的inode id，用于标识系统中唯一的inode
    pub(super) vfs_inode_id: InodeId,
}

#[derive(Debug)]
pub struct LockedExt4Inode(pub(super) SpinLock<Ext4Inode>);

impl IndexNode for LockedExt4Inode {
    fn open(
        &self,
        _data: crate::libs::spinlock::SpinLockGuard<vfs::FilePrivateData>,
        _mode: &vfs::file::FileMode,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn create(
        &self,
        name: &str,
        file_type: vfs::FileType,
        mode: vfs::syscall::ModeType,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut guard = self.0.lock();
        // another_ext4的高4位是文件类型，低12位是权限
        let file_mode = ModeType::from(file_type).union(mode);
        let ext4 = &guard.concret_fs().fs;
        let id = ext4.create(
            guard.inner_inode_num,
            name,
            another_ext4::InodeMode::from_bits_truncate(file_mode.bits() as u16),
        )?;
        let dname = DName::from(name);
        let inode = LockedExt4Inode::new(id, guard.fs_ptr.clone(), dname.clone());
        // 更新 children 缓存
        guard.children.insert(dname, inode.clone());
        drop(guard);
        Ok(inode as Arc<dyn IndexNode>)
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: vfs::FileType,
        mode: ModeType,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if data == 0 {
            return self.create(name, file_type, mode);
        }

        Err(SystemError::ENOSYS)
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: PrivateData,
    ) -> Result<usize, SystemError> {
        let guard = self.0.lock();

        let len = core::cmp::min(len, buf.len());
        let buf = &mut buf[0..len];
        let ext4 = &guard.concret_fs().fs;
        if let Some(page_cache) = &guard.page_cache {
            let time = PosixTimeSpec::now().tv_sec.to_u32().unwrap_or_else(|| {
                log::warn!("Failed to get current time, using 0");
                0
            });
            ext4.setattr(
                guard.inner_inode_num,
                None,
                None,
                None,
                None,
                Some(time),
                None,
                None,
                None,
            )
            .map_err(SystemError::from)?;
            page_cache.lock_irqsave().read(offset, buf)
        } else {
            self.read_direct(offset, len, buf, data)
        }
    }

    fn read_sync(&self, offset: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let guard = self.0.lock();
        let ext4 = &guard.concret_fs().fs;
        let inode_num = guard.inner_inode_num;
        match ext4.getattr(inode_num)?.ftype {
            FileType::Directory => Err(SystemError::EISDIR),
            FileType::Unknown => Err(SystemError::EROFS),
            FileType::RegularFile => ext4.read(inode_num, offset, buf).map_err(From::from),
            _ => Err(SystemError::EINVAL),
        }
    }

    fn read_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: crate::libs::spinlock::SpinLockGuard<vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let len = core::cmp::min(len, buf.len());
        self.read_sync(offset, &mut buf[0..len])
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: PrivateData,
    ) -> Result<usize, SystemError> {
        let guard = self.0.lock();
        let ext4 = &guard.concret_fs().fs;
        let len = core::cmp::min(len, buf.len());
        let buf = &buf[0..len];
        if let Some(page_cache) = &guard.page_cache {
            let write_len = page_cache.lock_irqsave().write(offset, buf)?;
            let old_file_size = ext4.getattr(guard.inner_inode_num)?.size;
            let current_file_size = core::cmp::max(old_file_size, (offset + write_len) as u64);
            let time = PosixTimeSpec::now().tv_sec.to_u32().unwrap_or_else(|| {
                log::warn!("Failed to get current time, using 0");
                0
            });
            ext4.setattr(
                guard.inner_inode_num,
                None,
                None,
                None,
                Some(current_file_size),
                None,
                Some(time),
                None,
                None,
            )
            .map_err(SystemError::from)?;
            Ok(write_len)
        } else {
            self.write_direct(offset, len, buf, data)
        }
    }

    fn write_sync(&self, offset: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let guard = self.0.lock();
        let ext4 = &guard.concret_fs().fs;
        let inode_num = guard.inner_inode_num;
        match ext4.getattr(inode_num)?.ftype {
            FileType::Directory => Err(SystemError::EISDIR),
            FileType::Unknown => Err(SystemError::EROFS),
            FileType::RegularFile => ext4.write(inode_num, offset, buf).map_err(From::from),
            _ => Err(SystemError::EINVAL),
        }
    }

    fn write_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let len = core::cmp::min(len, buf.len());
        self.write_sync(offset, &buf[0..len])
    }

    fn fs(&self) -> Arc<dyn vfs::FileSystem> {
        self.0.lock().concret_fs()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut guard = self.0.lock();
        let dname = DName::from(name);
        if let Some(child) = guard.children.get(&dname) {
            return Ok(child.clone() as Arc<dyn IndexNode>);
        }
        let next_inode = guard.concret_fs().fs.lookup(guard.inner_inode_num, name)?;
        let inode = LockedExt4Inode::new(next_inode, guard.fs_ptr.clone(), dname.clone());
        guard.children.insert(dname, inode.clone());
        Ok(inode)
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        let guard = self.0.lock();
        let dentry = guard.concret_fs().fs.listdir(guard.inner_inode_num)?;
        let mut list = Vec::new();
        for entry in dentry {
            list.push(entry.name());
        }
        Ok(list)
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let mut guard = self.0.lock();
        let ext4 = &guard.concret_fs().fs;
        let inode_num = guard.inner_inode_num;

        let other_arc = other
            .clone()
            .downcast_arc::<LockedExt4Inode>()
            .ok_or(SystemError::EINVAL)?;
        let other = other
            .downcast_ref::<LockedExt4Inode>()
            .ok_or(SystemError::EPERM)?;
        let other_inode_num = other.0.lock().inner_inode_num;

        let my_attr = ext4.getattr(inode_num)?;
        let other_attr = ext4.getattr(other_inode_num)?;

        if my_attr.ftype != another_ext4::FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }

        if other_attr.ftype == another_ext4::FileType::Directory {
            return Err(SystemError::EISDIR);
        }

        if ext4.lookup(inode_num, name).is_ok() {
            return Err(SystemError::EEXIST);
        }

        ext4.link(other_inode_num, inode_num, name)?;

        let dname = DName::from(name);
        guard.children.insert(dname, other_arc);

        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let mut guard = self.0.lock();
        let ext4 = &guard.concret_fs().fs;
        let inode_num = guard.inner_inode_num;
        let attr = ext4.getattr(inode_num)?;
        if attr.ftype != another_ext4::FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }
        ext4.unlink(inode_num, name)?;
        // 清理 children 缓存
        let _ = guard.children.remove(&DName::from(name));
        Ok(())
    }

    fn metadata(&self) -> Result<vfs::Metadata, SystemError> {
        let guard = self.0.lock();
        let ext4 = &guard.concret_fs().fs;
        let attr = ext4.getattr(guard.inner_inode_num)?;
        let raw_dev = guard.fs_ptr.upgrade().unwrap().raw_dev;
        Ok(vfs::Metadata {
            inode_id: guard.vfs_inode_id,
            size: attr.size as i64,
            blk_size: another_ext4::BLOCK_SIZE,
            blocks: attr.blocks as usize,
            atime: PosixTimeSpec::new(attr.atime.into(), 0),
            btime: PosixTimeSpec::new(attr.atime.into(), 0),
            mtime: PosixTimeSpec::new(attr.mtime.into(), 0),
            ctime: PosixTimeSpec::new(attr.ctime.into(), 0),
            file_type: Self::file_type(attr.ftype),
            mode: ModeType::from_bits_truncate(attr.perm.bits() as u32),
            nlinks: attr.links as usize,
            uid: attr.uid as usize,
            gid: attr.gid as usize,
            dev_id: 0,
            raw_dev,
        })
    }

    fn close(&self, _: PrivateData) -> Result<(), SystemError> {
        Ok(())
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        self.0.lock().page_cache.clone()
    }

    fn set_metadata(&self, metadata: &vfs::Metadata) -> Result<(), SystemError> {
        use another_ext4::InodeMode;
        let mode = metadata.mode.union(ModeType::from(metadata.file_type));

        let to_ext4_time =
            |time: &PosixTimeSpec| -> u32 { time.tv_sec.max(0).min(u32::MAX as i64) as u32 };

        let guard = self.0.lock();
        let ext4 = &guard.concret_fs().fs;
        ext4.setattr(
            guard.inner_inode_num,
            Some(InodeMode::from_bits_truncate(mode.bits() as u16)),
            Some(metadata.uid as u32),
            Some(metadata.gid as u32),
            Some(metadata.size as u64),
            Some(to_ext4_time(&metadata.atime)),
            Some(to_ext4_time(&metadata.mtime)),
            Some(to_ext4_time(&metadata.ctime)),
            Some(to_ext4_time(&metadata.btime)),
        )?;

        Ok(())
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        let guard = self.0.lock();
        let ext4 = &guard.concret_fs().fs;
        // 仅调整文件大小，其他属性保持不变
        ext4.setattr(
            guard.inner_inode_num,
            None,
            None,
            None,
            Some(len as u64),
            None,
            None,
            None,
            None,
        )
        .map_err(SystemError::from)?;
        Ok(())
    }

    fn truncate(&self, len: usize) -> Result<(), SystemError> {
        // 复用 resize 的实现
        self.resize(len)
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        let mut guard = self.0.lock();
        let concret_fs = &guard.concret_fs().fs;
        let inode_num = guard.inner_inode_num;
        if concret_fs.getattr(inode_num)?.ftype != FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }
        concret_fs.rmdir(inode_num, name)?;
        // 清理 children 缓存
        let _ = guard.children.remove(&DName::from(name));

        Ok(())
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(self.0.lock().dname.clone())
    }

    fn getxattr(&self, name: &str, buf: &mut [u8]) -> Result<usize, SystemError> {
        let guard = self.0.lock();
        let ext4 = &guard.concret_fs().fs;
        let inode_num = guard.inner_inode_num;

        if ext4.getattr(inode_num)?.ftype == FileType::SymLink {
            return Err(SystemError::EPERM);
        }

        // 调用another_ext4库的getxattr接口
        let value = ext4.getxattr(inode_num, name)?;

        // 如果缓冲区为空，只返回需要的长度
        if buf.is_empty() {
            return Ok(value.len());
        }

        // 检查缓冲区大小是否足够
        if buf.len() < value.len() {
            return Err(SystemError::ERANGE);
        }

        // 复制数据到缓冲区
        let copy_len = core::cmp::min(buf.len(), value.len());
        buf[..copy_len].copy_from_slice(&value[..copy_len]);

        Ok(copy_len)
    }

    fn setxattr(&self, name: &str, value: &[u8]) -> Result<usize, SystemError> {
        let guard = self.0.lock();
        let ext4 = &guard.concret_fs().fs;
        let inode_num = guard.inner_inode_num;

        if ext4.getattr(inode_num)?.ftype == FileType::SymLink {
            return Err(SystemError::EPERM);
        }

        if ext4.getxattr(inode_num, name).is_ok() {
            ext4.removexattr(inode_num, name)?;
        }

        // 调用another_ext4库的setxattr接口
        ext4.setxattr(inode_num, name, value)?;

        Ok(0)
    }
}

impl LockedExt4Inode {
    pub fn new(
        inode_num: u32,
        fs_ptr: Weak<super::filesystem::Ext4FileSystem>,
        dname: DName,
    ) -> Arc<Self> {
        let inode = Arc::new(LockedExt4Inode(SpinLock::new(Ext4Inode::new(
            inode_num, fs_ptr, dname,
        ))));
        let mut guard = inode.0.lock();

        let page_cache = PageCache::new(Some(Arc::downgrade(&inode) as Weak<dyn IndexNode>));
        guard.page_cache = Some(page_cache);

        drop(guard);
        return inode;
    }

    fn file_type(ftype: FileType) -> vfs::FileType {
        match ftype {
            FileType::RegularFile => vfs::FileType::File,
            FileType::Directory => vfs::FileType::Dir,
            FileType::CharacterDev => vfs::FileType::CharDevice,
            FileType::BlockDev => vfs::FileType::BlockDevice,
            FileType::Fifo => vfs::FileType::Pipe,
            FileType::Socket => vfs::FileType::Socket,
            FileType::SymLink => vfs::FileType::SymLink,
            _ => {
                log::warn!("Unknown file type, going to treat it as a file");
                vfs::FileType::File
            }
        }
    }
}

impl Ext4Inode {
    fn concret_fs(&self) -> Arc<Ext4FileSystem> {
        self.fs_ptr
            .upgrade()
            .expect("Ext4FileSystem should be alive")
    }

    pub fn new(inode_num: u32, fs_ptr: Weak<Ext4FileSystem>, dname: DName) -> Self {
        Self {
            inner_inode_num: inode_num,
            fs_ptr,
            page_cache: None,
            children: BTreeMap::new(),
            dname,
            vfs_inode_id: generate_inode_id(),
        }
    }
}

impl Debug for Ext4Inode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Ext4Inode")
    }
}
