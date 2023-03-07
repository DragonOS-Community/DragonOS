use alloc::{
    string::String,
    sync::{Arc, Weak},
};

use crate::{
    filesystem::{
        devfs::{devfs_register, DevFS, DeviceINode},
        vfs::{core::generate_inode_id, FileType, IndexNode, Metadata, PollStatus},
    },
    include::bindings::bindings::{vfs_file_operations_t, vfs_file_t, vfs_index_node_t, ENOTSUP},
    kdebug,
    libs::spinlock::SpinLock,
    time::TimeSpec,
};

#[derive(Debug)]
pub struct LockedKeyBoardInode(SpinLock<KeyBoardInode>);

#[derive(Debug)]
pub struct KeyBoardInode {
    /// uuid 暂时不知道有什么用（x
    // uuid: Uuid,
    /// 指向自身的弱引用
    self_ref: Weak<LockedKeyBoardInode>,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<DevFS>,
    /// INode 元数据
    metadata: Metadata,
    /// 键盘操作函数
    f_ops: vfs_file_operations_t,
}

impl LockedKeyBoardInode {
    pub fn new(f_ops: &vfs_file_operations_t) -> Arc<Self> {
        let inode = KeyBoardInode {
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
                mode: 0x666,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: 0, // 这里用来作为device number
            },
        };

        let result = Arc::new(LockedKeyBoardInode(SpinLock::new(inode)));
        result.0.lock().self_ref = Arc::downgrade(&result);

        return result;
    }
}

impl DeviceINode for LockedKeyBoardInode {
    fn set_fs(&self, fs: Weak<DevFS>) {
        self.0.lock().fs = fs;
    }
}

#[no_mangle] // 不重命名
pub extern "C" fn keyboard_register(f_ops: &vfs_file_operations_t) {
    kdebug!("register keyboard = {:p}", f_ops);
    devfs_register(
        String::from("ps2_keyboard"),
        LockedKeyBoardInode::new(f_ops),
    );
    kdebug!("register keyboard = {:p}", f_ops);
}

impl IndexNode for LockedKeyBoardInode {
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
        _buf: &mut [u8],
        _data: &mut crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, i32> {
        return Err(-(ENOTSUP as i32));
    }

    fn open(&self, _data: &mut crate::filesystem::vfs::FilePrivateData) -> Result<(), i32> {
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
