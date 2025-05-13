use crate::filesystem::vfs::{self, syscall::ModeType, IndexNode, InodeId};
use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use another_ext4;
use core::fmt::Debug;
use system_error::SystemError;

type PrivateData<'a> = crate::libs::spinlock::SpinLockGuard<'a, vfs::FilePrivateData>;

pub struct Ext4Inode {
    inode: u32,
    fs_ptr: Arc<super::filesystem::Ext4FileSystem>,
}

impl IndexNode for Ext4Inode {
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
        _file_type: vfs::FileType,
        mode: vfs::syscall::ModeType,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let id = self.concret_fs().create(
            self.inode,
            name,
            another_ext4::InodeMode::from_bits_truncate(mode.bits() as u16),
        )?;
        Ok(self.new_ref(id))
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _: PrivateData,
    ) -> Result<usize, SystemError> {
        self.concret_fs()
            .read(self.inode, offset, &mut buf[0..len])
            .map_err(From::from)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _: PrivateData,
    ) -> Result<usize, SystemError> {
        self.concret_fs()
            .write(self.inode, offset, &buf[0..len])
            .map_err(From::from)
    }

    fn fs(&self) -> Arc<dyn vfs::FileSystem> {
        self.fs_ptr.clone()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let next_inode = self.concret_fs().lookup(self.inode, name)?;
        Ok(self.new_ref(next_inode))
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        let dentry = self.concret_fs().listdir(self.inode)?;
        let mut list = Vec::new();
        for entry in dentry {
            list.push(entry.name());
        }
        Ok(list)
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        let other = other
            .downcast_ref::<Ext4Inode>()
            .ok_or(SystemError::EPERM)?;

        let my_attr = self.concret_fs().getattr(self.inode)?;
        let other_attr = self.concret_fs().getattr(other.inode)?;

        if my_attr.ftype != another_ext4::FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }

        if other_attr.ftype == another_ext4::FileType::Directory {
            return Err(SystemError::EISDIR);
        }

        if self.concret_fs().lookup(self.inode, name).is_ok() {
            return Err(SystemError::EEXIST);
        }

        self.concret_fs().link(self.inode, other.inode, name)?;
        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        let attr = self.concret_fs().getattr(self.inode)?;
        if attr.ftype != another_ext4::FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }
        self.concret_fs().unlink(self.inode, name)?;
        Ok(())
    }

    fn metadata(&self) -> Result<vfs::Metadata, SystemError> {
        let attr = self.concret_fs().getattr(self.inode)?;
        use crate::time::PosixTimeSpec;
        use another_ext4::FileType::*;
        Ok(vfs::Metadata {
            inode_id: InodeId::from(attr.ino as usize),
            size: attr.size as i64,
            blk_size: another_ext4::BLOCK_SIZE,
            blocks: attr.blocks as usize,
            atime: PosixTimeSpec::new(attr.atime.into(), 0),
            btime: PosixTimeSpec::new(attr.atime.into(), 0),
            mtime: PosixTimeSpec::new(attr.mtime.into(), 0),
            ctime: PosixTimeSpec::new(attr.ctime.into(), 0),
            file_type: match attr.ftype {
                RegularFile => vfs::FileType::File,
                Directory => vfs::FileType::Dir,
                CharacterDev => vfs::FileType::CharDevice,
                BlockDev => vfs::FileType::BlockDevice,
                Fifo => vfs::FileType::Pipe,
                Socket => vfs::FileType::Socket,
                SymLink => vfs::FileType::SymLink,
                Unknown => {
                    log::warn!("Unknown file type, going to treat it as a file");
                    vfs::FileType::File
                }
            },
            mode: ModeType::from_bits_truncate(attr.perm.bits() as u32),
            nlinks: attr.links as usize,
            uid: attr.uid as usize,
            gid: attr.gid as usize,
            dev_id: 0,
            raw_dev: crate::driver::base::device::device_number::DeviceNumber::default(),
        })
    }

    fn close(&self, _: PrivateData) -> Result<(), SystemError> {
        Ok(())
    }
}

impl Ext4Inode {
    pub fn new_ref(&self, inode: u32) -> Arc<Self> {
        Arc::new(Ext4Inode {
            inode,
            fs_ptr: self.fs_ptr.clone(),
        })
    }

    fn concret_fs(&self) -> &another_ext4::Ext4 {
        &self.fs_ptr.fs
    }

    pub(super) fn point_to_root(fs: Weak<super::filesystem::Ext4FileSystem>) -> Arc<Self> {
        Arc::new(Ext4Inode {
            inode: another_ext4::EXT4_ROOT_INO,
            fs_ptr: fs.upgrade().expect("Ext4FileSystem should be alive"),
        })
    }
}

impl Debug for Ext4Inode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Ext4Inode")
    }
}
