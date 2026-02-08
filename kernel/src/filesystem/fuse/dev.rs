use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};

use system_error::SystemError;

use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::{
        devfs::{DevFS, DeviceINode, LockedDevFSInode},
        epoll::EPollItem,
        vfs::{
            file::FileFlags, file::FuseDevPrivateData, vcore::generate_inode_id, FilePrivateData,
            FileSystem, FileType, IndexNode, InodeFlags, InodeMode, Metadata, PollableInode,
        },
    },
    libs::mutex::{Mutex, MutexGuard},
    process::ProcessManager,
    syscall::user_access::UserBufferReader,
    time::PosixTimeSpec,
};

use super::conn::FuseConn;
const FUSE_DEV_IOC_CLONE: u32 = 0x8004_4600; // _IOR('F', 0, uint32_t)

#[derive(Debug)]
pub struct FuseDevInode {
    self_ref: Weak<LockedFuseDevInode>,
    fs: Weak<DevFS>,
    parent: Weak<LockedDevFSInode>,
    metadata: Metadata,
}

#[derive(Debug)]
pub struct LockedFuseDevInode(Mutex<FuseDevInode>);

impl LockedFuseDevInode {
    pub fn new() -> Arc<Self> {
        let inode = FuseDevInode {
            self_ref: Weak::default(),
            fs: Weak::default(),
            parent: Weak::default(),
            metadata: Metadata {
                dev_id: 1,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                btime: PosixTimeSpec::default(),
                file_type: FileType::CharDevice,
                mode: InodeMode::from_bits_truncate(0o666),
                flags: InodeFlags::empty(),
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::default(),
            },
        };
        let result = Arc::new(LockedFuseDevInode(Mutex::new(inode)));
        result.0.lock().self_ref = Arc::downgrade(&result);
        result
    }
}

impl DeviceINode for LockedFuseDevInode {
    fn set_fs(&self, fs: Weak<DevFS>) {
        self.0.lock().fs = fs;
    }

    fn set_parent(&self, parent: Weak<LockedDevFSInode>) {
        self.0.lock().parent = parent;
    }
}

impl PollableInode for LockedFuseDevInode {
    fn poll(&self, private_data: &FilePrivateData) -> Result<usize, SystemError> {
        let FilePrivateData::FuseDev(p) = private_data else {
            return Err(SystemError::EINVAL);
        };
        let conn = p
            .conn
            .clone()
            .downcast::<FuseConn>()
            .map_err(|_| SystemError::EINVAL)?;
        Ok(conn.poll().bits() as usize)
    }

    fn add_epitem(
        &self,
        epitem: Arc<EPollItem>,
        private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let FilePrivateData::FuseDev(p) = private_data else {
            return Err(SystemError::EINVAL);
        };
        let conn = p
            .conn
            .clone()
            .downcast::<FuseConn>()
            .map_err(|_| SystemError::EINVAL)?;
        conn.add_epitem(epitem)
    }

    fn remove_epitem(
        &self,
        epitem: &Arc<EPollItem>,
        private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let FilePrivateData::FuseDev(p) = private_data else {
            return Err(SystemError::EINVAL);
        };
        let conn = p
            .conn
            .clone()
            .downcast::<FuseConn>()
            .map_err(|_| SystemError::EINVAL)?;
        conn.remove_epitem(epitem)
    }
}

impl IndexNode for LockedFuseDevInode {
    fn is_stream(&self) -> bool {
        true
    }

    fn open(
        &self,
        mut data: MutexGuard<FilePrivateData>,
        flags: &FileFlags,
    ) -> Result<(), SystemError> {
        let nonblock = flags.contains(FileFlags::O_NONBLOCK);
        let conn = FuseConn::new();
        let conn_any: Arc<dyn core::any::Any + Send + Sync> = conn;
        *data = FilePrivateData::FuseDev(FuseDevPrivateData {
            conn: conn_any,
            nonblock,
        });
        Ok(())
    }

    fn close(&self, data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        if let FilePrivateData::FuseDev(p) = &*data {
            if let Ok(conn) = p.conn.clone().downcast::<FuseConn>() {
                conn.dev_release();
            }
        }
        Ok(())
    }

    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        let (conn_any, nonblock) = {
            let FilePrivateData::FuseDev(p) = &*data else {
                return Err(SystemError::EINVAL);
            };
            (p.conn.clone(), p.nonblock)
        };
        // Drop private_data lock before potentially blocking in read_request().
        drop(data);
        let conn = conn_any
            .downcast::<FuseConn>()
            .map_err(|_| SystemError::EINVAL)?;
        conn.read_request(nonblock, &mut buf[..len])
    }

    fn write_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        let conn_any = {
            let FilePrivateData::FuseDev(p) = &*data else {
                return Err(SystemError::EINVAL);
            };
            p.conn.clone()
        };
        // Drop private_data lock before potentially blocking in write_reply().
        drop(data);
        let conn = conn_any
            .downcast::<FuseConn>()
            .map_err(|_| SystemError::EINVAL)?;
        conn.write_reply(&buf[..len])
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.0.lock().metadata.clone())
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        inode.metadata.atime = metadata.atime;
        inode.metadata.mtime = metadata.mtime;
        inode.metadata.ctime = metadata.ctime;
        inode.metadata.btime = metadata.btime;
        inode.metadata.mode = metadata.mode;
        inode.metadata.uid = metadata.uid;
        inode.metadata.gid = metadata.gid;
        Ok(())
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.0.lock().fs.upgrade().unwrap()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        Err(SystemError::EINVAL)
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        let parent = self.0.lock().parent.upgrade().ok_or(SystemError::ENOENT)?;
        Ok(parent)
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Ok(self)
    }

    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        mut private_data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        match cmd {
            FUSE_DEV_IOC_CLONE => {
                if data == 0 {
                    return Err(SystemError::EFAULT);
                }

                let reader =
                    UserBufferReader::new(data as *const u32, core::mem::size_of::<u32>(), true)?;
                let oldfd = reader.buffer_protected(0)?.read_one::<u32>(0)? as i32;

                let old_file = ProcessManager::current_pcb()
                    .fd_table()
                    .read()
                    .get_file_by_fd(oldfd)
                    .ok_or(SystemError::EINVAL)?;

                let old_conn = {
                    let guard = old_file.private_data.lock();
                    let FilePrivateData::FuseDev(p) = &*guard else {
                        return Err(SystemError::EINVAL);
                    };
                    p.conn.clone()
                };

                let FilePrivateData::FuseDev(p) = &mut *private_data else {
                    return Err(SystemError::EINVAL);
                };
                let old_fc = old_conn
                    .clone()
                    .downcast::<FuseConn>()
                    .map_err(|_| SystemError::EINVAL)?;

                // If this fd already points to the same connection, this is a no-op.
                if let Ok(cur_fc) = p.conn.clone().downcast::<FuseConn>() {
                    if Arc::ptr_eq(&cur_fc, &old_fc) {
                        return Ok(0);
                    }
                    cur_fc.dev_release();
                }

                old_fc.dev_acquire();
                p.conn = old_conn;

                Ok(0)
            }
            _ => Err(SystemError::ENOTTY),
        }
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(String::from("/dev/fuse"))
    }
}
