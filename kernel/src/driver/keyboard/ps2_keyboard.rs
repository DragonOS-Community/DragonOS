use alloc::sync::{Arc, Weak};

use crate::{
    filesystem::{
        devfs::{devfs_register, DevFS, DeviceINode},
        vfs::{core::generate_inode_id, file::FileMode, FileType, IndexNode, Metadata, PollStatus},
    },
    include::bindings::bindings::{vfs_file_operations_t, vfs_file_t, vfs_index_node_t, ENOTSUP},
    kdebug,
    libs::spinlock::SpinLock,
    time::TimeSpec,
};

#[derive(Debug)]
pub struct LockedPS2KeyBoardInode(SpinLock<PS2KeyBoardInode>);

#[derive(Debug)]
pub struct PS2KeyBoardInode {
    /// uuid 暂时不知道有什么用（x
    // uuid: Uuid,
    /// 指向自身的弱引用
    self_ref: Weak<LockedPS2KeyBoardInode>,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<DevFS>,
    /// INode 元数据
    metadata: Metadata,
    /// 键盘操作函数
    f_ops: vfs_file_operations_t,
}

impl LockedPS2KeyBoardInode {
    pub fn new(f_ops: &vfs_file_operations_t) -> Arc<Self> {
        let inode = PS2KeyBoardInode {
            // uuid: Uuid::new_v5(),
            self_ref: Weak::default(),
            fs: Weak::default(),
            f_ops: f_ops.clone(), // 从引用复制一遍获取所有权
            metadata: Metadata {
                dev_id: 1,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: TimeSpec::default(),
                mtime: TimeSpec::default(),
                ctime: TimeSpec::default(),
                file_type: FileType::CharDevice, // 文件夹，block设备，char设备
                mode: 0o666,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: 0, // 这里用来作为device number
            },
        };

        let result = Arc::new(LockedPS2KeyBoardInode(SpinLock::new(inode)));
        result.0.lock().self_ref = Arc::downgrade(&result);

        return result;
    }
}

impl DeviceINode for LockedPS2KeyBoardInode {
    fn set_fs(&self, fs: Weak<DevFS>) {
        self.0.lock().fs = fs;
    }
}

#[no_mangle] // 不重命名
pub extern "C" fn ps2_keyboard_register(f_ops: &vfs_file_operations_t) {
    devfs_register("ps2_keyboard", LockedPS2KeyBoardInode::new(f_ops))
        .expect("Failed to register ps/2 keyboard");
}

impl IndexNode for LockedPS2KeyBoardInode {
    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, i32> {
        let guard = self.0.lock();
        let func = guard.f_ops.read.unwrap();
        let r = unsafe {
            func(
                0 as *mut vfs_file_t,
                &mut buf[0..len] as *mut [u8] as *mut i8,
                len as i64,
                0 as *mut i64,
            )
        };
        return Ok(r as usize);
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: &mut crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, i32> {
        return Err(-(ENOTSUP as i32));
    }

    fn open(
        &self,
        _data: &mut crate::filesystem::vfs::FilePrivateData,
        _mode: &FileMode,
    ) -> Result<(), i32> {
        let guard = self.0.lock();
        let func = guard.f_ops.open.unwrap();
        let _ = unsafe { func(0 as *mut vfs_index_node_t, 0 as *mut vfs_file_t) };
        return Ok(());
    }

    fn close(&self, _data: &mut crate::filesystem::vfs::FilePrivateData) -> Result<(), i32> {
        let guard = self.0.lock();
        let func = guard.f_ops.close.unwrap();
        let _ = unsafe { func(0 as *mut vfs_index_node_t, 0 as *mut vfs_file_t) };
        return Ok(());
    }

    fn poll(&self) -> Result<PollStatus, i32> {
        return Ok(PollStatus {
            flags: PollStatus::READ_MASK,
        });
    }

    fn metadata(&self) -> Result<Metadata, i32> {
        return Ok(self.0.lock().metadata.clone());
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), i32> {
        let mut inode = self.0.lock();
        inode.metadata.atime = metadata.atime;
        inode.metadata.mtime = metadata.mtime;
        inode.metadata.ctime = metadata.ctime;
        inode.metadata.mode = metadata.mode;
        inode.metadata.uid = metadata.uid;
        inode.metadata.gid = metadata.gid;

        return Ok(());
    }

    fn fs(&self) -> alloc::sync::Arc<dyn crate::filesystem::vfs::FileSystem> {
        return self.0.lock().fs.upgrade().unwrap();
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, i32> {
        return Err(-(ENOTSUP as i32));
    }
}
