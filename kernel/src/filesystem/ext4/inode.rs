use crate::{
    filesystem::{
        page_cache::{AsyncPageCacheBackend, PageCache},
        vfs::{
            self, syscall::RenameFlags, utils::DName, vcore::generate_inode_id, FilePrivateData,
            IndexNode, InodeFlags, InodeId, InodeMode,
        },
    },
    libs::{
        casting::DowncastArc,
        mutex::{Mutex, MutexGuard},
    },
    mm::truncate::truncate_inode_pages,
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

type PrivateData<'a> = crate::libs::mutex::MutexGuard<'a, vfs::FilePrivateData>;

pub struct Ext4Inode {
    // 对应another_ext4里面的inode号，用于在ext4文件系统中查找相应的inode
    pub(super) inner_inode_num: u32,
    pub(super) fs_ptr: Weak<super::filesystem::Ext4FileSystem>,
    pub(super) page_cache: Option<Arc<PageCache>>,
    pub(super) children: BTreeMap<DName, Arc<LockedExt4Inode>>,
    pub(super) dname: DName,

    // 对应vfs的inode id，用于标识系统中唯一的inode
    pub(super) vfs_inode_id: InodeId,

    // 指向父级IndexNode的Weak指针
    pub(super) parent: Weak<LockedExt4Inode>,

    // 指向自身的Weak指针，用于获取Arc<Self>
    pub(super) self_ref: Weak<LockedExt4Inode>,
}

#[derive(Debug)]
pub struct LockedExt4Inode(pub(super) Mutex<Ext4Inode>);

impl IndexNode for LockedExt4Inode {
    fn mmap(&self, _start: usize, _len: usize, _offset: usize) -> Result<(), SystemError> {
        Ok(())
    }

    fn open(
        &self,
        _data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
        _mode: &vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn create(
        &self,
        name: &str,
        file_type: vfs::FileType,
        mode: vfs::InodeMode,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut guard = self.0.lock();
        // another_ext4的高4位是文件类型，低12位是权限
        let file_mode = InodeMode::from(file_type).union(mode);
        let ext4 = &guard.concret_fs().fs;
        let id = ext4.create(
            guard.inner_inode_num,
            name,
            another_ext4::InodeMode::from_bits_truncate(file_mode.bits() as u16),
        )?;
        let dname = DName::from(name);
        // 通过self_ref获取Arc<Self>，然后转换为Arc<dyn IndexNode>
        let self_arc = guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
        let inode = LockedExt4Inode::new(
            id,
            guard.fs_ptr.clone(),
            dname.clone(),
            Some(Arc::downgrade(&self_arc)),
        );
        // 更新 children 缓存
        guard.children.insert(dname, inode.clone());
        drop(guard);
        Ok(inode as Arc<dyn IndexNode>)
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: vfs::FileType,
        mode: InodeMode,
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
        let len = core::cmp::min(len, buf.len());
        let buf = &mut buf[0..len];

        // 关键修复：不要在持有 Ext4 inode 自旋锁期间调用 PageCache::{read,write}。
        // PageCache 读写路径内部会调用 inode.metadata() 获取文件大小：
        // - prepare_read(): inode.metadata()
        // 若此处持有 inode 锁，则会在 metadata() 再次尝试获取同一把锁而自旋死锁。
        let (fs, inode_num, page_cache) = {
            let guard = self.0.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.page_cache.clone(),
            )
        };

        if let Some(page_cache) = page_cache {
            let time = PosixTimeSpec::now().tv_sec.to_u32().unwrap_or_else(|| {
                log::warn!("Failed to get current time, using 0");
                0
            });
            fs.fs
                .setattr(
                    inode_num,
                    another_ext4::SetAttr {
                        mode: None,
                        uid: None,
                        gid: None,
                        size: None,
                        atime: Some(time),
                        mtime: None,
                        ctime: None,
                        crtime: None,
                    },
                )
                .map_err(SystemError::from)?;
            page_cache.read(offset, buf)
        } else {
            self.read_direct(offset, len, buf, data)
        }
    }

    fn read_sync(&self, offset: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let (fs, inode_num) = {
            let guard = self.0.lock();
            (guard.concret_fs(), guard.inner_inode_num)
        };
        match fs.fs.getattr(inode_num)?.ftype {
            FileType::Directory => Err(SystemError::EISDIR),
            FileType::Unknown => Err(SystemError::EROFS),
            FileType::RegularFile => fs.fs.read(inode_num, offset, buf).map_err(From::from),
            FileType::SymLink => fs.fs.readlink(inode_num, offset, buf).map_err(From::from),
            _ => Err(SystemError::EINVAL),
        }
    }

    fn read_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: crate::libs::mutex::MutexGuard<vfs::FilePrivateData>,
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
        let len = core::cmp::min(len, buf.len());
        let buf = &buf[0..len];

        let (fs, inode_num, page_cache) = {
            let guard = self.0.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.page_cache.clone(),
            )
        };

        if let Some(page_cache) = page_cache {
            let write_len = PageCache::write(&page_cache, offset, buf)?;
            let old_file_size = fs.fs.getattr(inode_num)?.size;
            let current_file_size = core::cmp::max(old_file_size, (offset + write_len) as u64);
            let time = PosixTimeSpec::now().tv_sec.to_u32().unwrap_or_else(|| {
                log::warn!("Failed to get current time, using 0");
                0
            });
            fs.fs
                .setattr(
                    inode_num,
                    another_ext4::SetAttr {
                        mode: None,
                        uid: None,
                        gid: None,
                        size: Some(current_file_size),
                        atime: None,
                        mtime: Some(time),
                        ctime: None,
                        crtime: None,
                    },
                )
                .map_err(SystemError::from)?;
            Ok(write_len)
        } else {
            self.write_direct(offset, len, buf, data)
        }
    }

    fn write_sync(&self, offset: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let (fs, inode_num) = {
            let guard = self.0.lock();
            (guard.concret_fs(), guard.inner_inode_num)
        };
        match fs.fs.getattr(inode_num)?.ftype {
            FileType::Directory => Err(SystemError::EISDIR),
            FileType::Unknown => Err(SystemError::EROFS),
            FileType::RegularFile => fs.fs.write(inode_num, offset, buf).map_err(From::from),
            _ => Err(SystemError::EINVAL),
        }
    }

    fn write_direct(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
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
        // 通过self_ref获取Arc<Self>，然后转换为Arc<dyn IndexNode>
        let self_arc = guard.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
        let inode = LockedExt4Inode::new(
            next_inode,
            guard.fs_ptr.clone(),
            dname.clone(),
            Some(Arc::downgrade(&self_arc)),
        );
        guard.children.insert(dname, inode.clone());
        Ok(inode)
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 只有目录才有父目录的概念
        // 先检查当前inode是否为目录
        let guard = self.0.lock();

        // 如果存储了父级指针，直接返回
        if let Some(parent) = guard.parent.upgrade() {
            return Ok(parent);
        }

        Err(SystemError::ENOENT)
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
        let other_inode_num = other_arc.0.lock().inner_inode_num;

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
        let (fs, inode_num, vfs_inode_id) = {
            let guard = self.0.lock();
            (
                guard.concret_fs(),
                guard.inner_inode_num,
                guard.vfs_inode_id,
            )
        };
        let attr = fs.fs.getattr(inode_num)?;
        let raw_dev = fs.raw_dev;
        Ok(vfs::Metadata {
            inode_id: vfs_inode_id,
            size: attr.size as i64,
            blk_size: another_ext4::BLOCK_SIZE,
            blocks: attr.blocks as usize,
            atime: PosixTimeSpec::new(attr.atime.into(), 0),
            btime: PosixTimeSpec::new(attr.atime.into(), 0),
            mtime: PosixTimeSpec::new(attr.mtime.into(), 0),
            ctime: PosixTimeSpec::new(attr.ctime.into(), 0),
            file_type: Self::file_type(attr.ftype),
            mode: InodeMode::from_bits_truncate(attr.perm.bits() as u32),
            flags: InodeFlags::empty(),
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
        let mode = metadata.mode.union(InodeMode::from(metadata.file_type));

        let to_ext4_time =
            |time: &PosixTimeSpec| -> u32 { time.tv_sec.max(0).min(u32::MAX as i64) as u32 };

        let guard = self.0.lock();
        let ext4 = &guard.concret_fs().fs;
        ext4.setattr(
            guard.inner_inode_num,
            another_ext4::SetAttr {
                mode: Some(another_ext4::InodeMode::from_bits_truncate(
                    mode.bits() as u16
                )),
                uid: Some(metadata.uid as u32),
                gid: Some(metadata.gid as u32),
                size: Some(metadata.size as u64),
                atime: Some(to_ext4_time(&metadata.atime)),
                mtime: Some(to_ext4_time(&metadata.mtime)),
                ctime: Some(to_ext4_time(&metadata.ctime)),
                crtime: Some(to_ext4_time(&metadata.btime)),
            },
        )?;

        Ok(())
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        let guard = self.0.lock();
        let ext4 = &guard.concret_fs().fs;
        // 仅调整文件大小，其他属性保持不变
        ext4.setattr(
            guard.inner_inode_num,
            another_ext4::SetAttr {
                mode: None,
                uid: None,
                gid: None,
                size: Some(len as u64),
                atime: None,
                mtime: None,
                ctime: None,
                crtime: None,
            },
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

    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
        flags: RenameFlags,
    ) -> Result<(), SystemError> {
        // 目标必须是 LockedExt4Inode（同一文件系统）
        let target_locked = target
            .clone()
            .downcast_arc::<LockedExt4Inode>()
            .ok_or(SystemError::EXDEV)?;

        let (ext4_fs, src_inode_num) = {
            let src_guard = self.0.lock();
            (src_guard.concret_fs(), src_guard.inner_inode_num)
        };
        let ext4 = &ext4_fs.fs;
        let target_inode_num = target_locked.0.lock().inner_inode_num;

        let old_dname = DName::from(old_name);
        let new_dname = DName::from(new_name);

        // 快速路径：同目录同名 = 无操作
        if src_inode_num == target_inode_num && old_dname == new_dname {
            return Ok(());
        }

        // ========== 阶段1: 底层 ext4 操作（无需持有缓存锁）==========
        // ext4 文件系统有自己的同步机制，这些操作可以安全地在锁外执行

        // 获取源文件信息，用于后续类型检查
        let src_child_id = ext4.lookup(src_inode_num, old_name)?;
        let src_attr = ext4.getattr(src_child_id)?;

        // 检查目标是否已存在，若存在需要先删除
        let dst_exists = if let Ok(dst_child_id) = ext4.lookup(target_inode_num, new_name) {
            if flags.contains(RenameFlags::NOREPLACE) {
                return Err(SystemError::EEXIST);
            }

            let dst_attr = ext4.getattr(dst_child_id)?;

            // POSIX rename 类型兼容性检查
            if src_attr.ftype == FileType::Directory {
                if dst_attr.ftype != FileType::Directory {
                    return Err(SystemError::ENOTDIR);
                }
                // 目标目录必须为空
                let entries = ext4.listdir(dst_child_id)?;
                let has_real_entries = entries.iter().any(|e| e.name() != "." && e.name() != "..");
                if has_real_entries {
                    return Err(SystemError::ENOTEMPTY);
                }
                ext4.rmdir(target_inode_num, new_name)?;
            } else {
                if dst_attr.ftype == FileType::Directory {
                    return Err(SystemError::EISDIR);
                }
                // 在删除前清理 page cache
                match target_locked.find(new_name) {
                    Ok(dst_inode) => {
                        if let Some(page_cache) = dst_inode.page_cache() {
                            truncate_inode_pages(page_cache, 0);
                        }
                    }
                    Err(SystemError::ENOENT) => {
                        // 文件不在缓存中，无需清理 page cache
                    }
                    Err(e) => {
                        // 其他错误（如 EIO、ENOMEM）不应静默忽略
                        log::error!(
                            "move_to: unexpected error finding '{}' for page cache cleanup: {:?}",
                            new_name,
                            e
                        );
                        return Err(e);
                    }
                }
                ext4.unlink(target_inode_num, new_name)?;
            }
            true
        } else {
            false
        };

        // 执行底层 ext4 重命名
        ext4.rename(src_inode_num, old_name, target_inode_num, new_name)?;

        // ========== 阶段2: 缓存更新（需要按顺序持有锁）==========
        // 锁排序：按 inode_num 顺序获取锁，避免死锁

        if src_inode_num == target_inode_num {
            let mut dir_guard = self.0.lock();
            if dst_exists {
                dir_guard.children.remove(&new_dname);
            }
            if let Some(child) = dir_guard.children.remove(&old_dname) {
                child.0.lock().dname = new_dname.clone();
                dir_guard.children.insert(new_dname, child);
            }
        } else {
            let (mut src_guard, mut dst_guard) = if src_inode_num < target_inode_num {
                (self.0.lock(), target_locked.0.lock())
            } else {
                // 按 inode_num 顺序获取锁，避免死锁
                let dst = target_locked.0.lock();
                let src = self.0.lock();
                (src, dst)
            };

            // 如果目标位置有被覆盖的条目，从缓存中移除
            if dst_exists {
                dst_guard.children.remove(&new_dname);
            }

            // 从源目录缓存移除，添加到目标目录缓存
            // 保存 child 引用以便后续更新，避免 drop 后重新查找的竞态
            let child_to_update = src_guard.children.remove(&old_dname).inspect(|child| {
                dst_guard.children.insert(new_dname.clone(), child.clone());
                // child
            });
            drop(src_guard);
            drop(dst_guard);

            if let Some(child) = child_to_update {
                let mut child_guard = child.0.lock();
                child_guard.dname = new_dname.clone();
                child_guard.parent = Arc::downgrade(&target_locked);
            }
        }
        Ok(())
    }
}

impl LockedExt4Inode {
    pub fn new(
        inode_num: u32,
        fs_ptr: Weak<super::filesystem::Ext4FileSystem>,
        dname: DName,
        parent: Option<Weak<LockedExt4Inode>>,
    ) -> Arc<Self> {
        let inode = Arc::new({
            LockedExt4Inode(Mutex::new(Ext4Inode::new(inode_num, fs_ptr, dname, parent)))
        });
        let mut guard = inode.0.lock();

        // 设置self_ref
        guard.self_ref = Arc::downgrade(&inode);

        let backend = Arc::new(AsyncPageCacheBackend::new(
            Arc::downgrade(&inode) as Weak<dyn IndexNode>
        ));
        let page_cache = PageCache::new(
            Some(Arc::downgrade(&inode) as Weak<dyn IndexNode>),
            Some(backend),
        );
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

    pub fn new(
        inode_num: u32,
        fs_ptr: Weak<Ext4FileSystem>,
        dname: DName,
        parent: Option<Weak<LockedExt4Inode>>,
    ) -> Self {
        Self {
            inner_inode_num: inode_num,
            fs_ptr,
            page_cache: None,
            children: BTreeMap::new(),
            dname,
            vfs_inode_id: generate_inode_id(),
            parent: parent.unwrap_or_default(),
            self_ref: Weak::new(), // 将在LockedExt4Inode::new()中设置
        }
    }
}

impl Debug for Ext4Inode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Ext4Inode")
    }
}
