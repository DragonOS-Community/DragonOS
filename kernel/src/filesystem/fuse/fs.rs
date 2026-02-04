use alloc::{string::String, sync::Arc, vec::Vec};

use system_error::SystemError;

use crate::{
    filesystem::vfs::{
        vcore::generate_inode_id, FileSystem, FileSystemMakerData, FileType, FsInfo, IndexNode,
        InodeFlags, InodeMode, Magic, Metadata, MountableFileSystem, SuperBlock, FSMAKER,
    },
    libs::mutex::{Mutex, MutexGuard},
    process::ProcessManager,
    register_mountable_fs,
    time::PosixTimeSpec,
};

use linkme::distributed_slice;

use super::conn::FuseConn;

#[derive(Debug)]
pub struct FuseMountData {
    pub fd: i32,
    pub rootmode: u32,
    pub user_id: u32,
    pub group_id: u32,
    pub conn: Arc<FuseConn>,
}

impl FileSystemMakerData for FuseMountData {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

#[derive(Debug)]
pub struct FuseFS {
    root: Arc<FuseRootInode>,
    super_block: SuperBlock,
    conn: Arc<FuseConn>,
    #[allow(dead_code)]
    owner_uid: u32,
    #[allow(dead_code)]
    owner_gid: u32,
}

impl FuseFS {
    fn parse_mount_options(raw: Option<&str>) -> Result<(i32, u32, u32, u32), SystemError> {
        let mut fd: Option<i32> = None;
        let mut rootmode: Option<u32> = None;
        let mut user_id: Option<u32> = None;
        let mut group_id: Option<u32> = None;

        let s = raw.unwrap_or("");
        for part in s.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (k, v) = match part.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => (part, ""),
            };
            match k {
                "fd" => {
                    fd = Some(v.parse::<i32>().map_err(|_| SystemError::EINVAL)?);
                }
                "rootmode" => {
                    // Linux expects octal representation.
                    rootmode = Some(u32::from_str_radix(v, 8).map_err(|_| SystemError::EINVAL)?);
                }
                "user_id" => {
                    user_id = Some(v.parse::<u32>().map_err(|_| SystemError::EINVAL)?);
                }
                "group_id" => {
                    group_id = Some(v.parse::<u32>().map_err(|_| SystemError::EINVAL)?);
                }
                _ => {}
            }
        }

        let fd = fd.ok_or(SystemError::EINVAL)?;
        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred();
        let user_id = user_id.unwrap_or(cred.fsuid.data() as u32);
        let group_id = group_id.unwrap_or(cred.fsgid.data() as u32);
        // Default root mode: directory 0755 (with type bit).
        let rootmode = rootmode.unwrap_or(0o040755);

        Ok((fd, rootmode, user_id, group_id))
    }
}

impl MountableFileSystem for FuseFS {
    fn make_mount_data(
        raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        let (fd, rootmode, user_id, group_id) = Self::parse_mount_options(raw_data)?;

        let file = ProcessManager::current_pcb()
            .fd_table()
            .read()
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;

        let conn = {
            let pdata = file.private_data.lock();
            match &*pdata {
                crate::filesystem::vfs::FilePrivateData::FuseDev(p) => p
                    .conn
                    .clone()
                    .downcast::<FuseConn>()
                    .map_err(|_| SystemError::EINVAL)?,
                _ => return Err(SystemError::EINVAL),
            }
        };

        conn.mark_mounted()?;

        Ok(Some(Arc::new(FuseMountData {
            fd,
            rootmode,
            user_id,
            group_id,
            conn,
        })))
    }

    fn make_fs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let mount_data = data
            .and_then(|d| d.as_any().downcast_ref::<FuseMountData>())
            .ok_or(SystemError::EINVAL)?;

        let super_block = SuperBlock::new(Magic::FUSE_MAGIC, 4096, 255);

        let root_md = Metadata {
            dev_id: 0,
            inode_id: generate_inode_id(),
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: PosixTimeSpec::default(),
            mtime: PosixTimeSpec::default(),
            ctime: PosixTimeSpec::default(),
            btime: PosixTimeSpec::default(),
            file_type: FileType::Dir,
            mode: InodeMode::from_bits_truncate(mount_data.rootmode),
            flags: InodeFlags::empty(),
            nlinks: 2,
            uid: mount_data.user_id as usize,
            gid: mount_data.group_id as usize,
            raw_dev: crate::driver::base::device::device_number::DeviceNumber::default(),
        };

        let conn = mount_data.conn.clone();

        let fs = Arc::new_cyclic(|weak_fs| {
            let root = Arc::new_cyclic(|weak_root| FuseRootInode {
                self_ref: weak_root.clone(),
                fs: weak_fs.clone(),
                metadata: Mutex::new(root_md),
            });
            FuseFS {
                root,
                super_block,
                conn: conn.clone(),
                owner_uid: mount_data.user_id,
                owner_gid: mount_data.group_id,
            }
        });

        conn.enqueue_init()?;
        Ok(fs)
    }
}

register_mountable_fs!(FuseFS, FUSEFSMAKER, "fuse");

impl FileSystem for FuseFS {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.root.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: 255,
        }
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn name(&self) -> &str {
        "fuse"
    }

    fn super_block(&self) -> SuperBlock {
        self.super_block.clone()
    }
}

#[derive(Debug)]
pub struct FuseRootInode {
    self_ref: alloc::sync::Weak<FuseRootInode>,
    fs: alloc::sync::Weak<FuseFS>,
    metadata: Mutex<Metadata>,
}

impl IndexNode for FuseRootInode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn open(
        &self,
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
        _flags: &crate::filesystem::vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn close(
        &self,
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EISDIR)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EISDIR)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.metadata.lock().clone())
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        *self.metadata.lock() = metadata.clone();
        Ok(())
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs.upgrade().unwrap()
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        Ok(Vec::new())
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        match name {
            "." | ".." => Ok(self.self_ref.upgrade().ok_or(SystemError::ENOENT)?),
            _ => Err(SystemError::ENOENT),
        }
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        Ok(self.self_ref.upgrade().ok_or(SystemError::ENOENT)?)
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(String::from("/"))
    }
}
