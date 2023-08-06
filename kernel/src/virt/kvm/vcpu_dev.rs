use crate::kdebug;
use crate::filesystem::devfs::{DevFS, DeviceINode};
use crate::filesystem::vfs::{
    core::{generate_inode_id},
    file::{File, FileMode},
    FileSystem, FilePrivateData, FileType, IndexNode, Metadata, PollStatus,
    make_rawdev, ROOT_INODE
};
use crate::{
    arch::asm::current::current_pcb,
    libs::spinlock::{SpinLock},
    syscall::SystemError,
    time::TimeSpec,
    arch::KVMArch,
};
use super::Hypervisor;
use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
    boxed::Box,
};

pub const KVM_API_VERSION:u32 = 12;

pub const GUEST_STACK_SIZE:usize = 1024;
pub const HOST_STACK_SIZE:usize = 0x1000 * 6;

/*
 * ioctls for /dev/vm fds:
 */
pub const KVM_CREATE_VCPU: u32 = 0x00;
pub const KVM_SET_USER_MEMORY_REGION: u32 = 0x01;
pub const KVM_GET_DIRTY_LOG: u32 = 0x02;
pub const KVM_IRQFD: u32 = 0x03;
pub const KVM_IOEVENTFD: u32 = 0x04;
pub const KVM_IRQ_LINE_STATUS: u32 = 0x05;

//  #[derive(Debug)]
//  pub struct InodeInfo {
//     kvm: Arc<Hypervisor>,
//  }
 
#[derive(Debug)]
pub struct VcpuInode {
    /// uuid 暂时不知道有什么用（x
    // uuid: Uuid,
    /// 指向自身的弱引用
    self_ref: Weak<LockedVcpuInode>,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<DevFS>,
    /// INode 元数据
    metadata: Metadata,
    // fdata: InodeInfo,
}

#[derive(Debug)]
pub struct LockedVcpuInode(SpinLock<VcpuInode>);

impl LockedVcpuInode {
    pub fn new() -> Arc<Self> {
        let inode = VcpuInode {
            self_ref: Weak::default(),
            fs: Weak::default(),
            metadata: Metadata {
                dev_id: 1,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: TimeSpec::default(),
                mtime: TimeSpec::default(),
                ctime: TimeSpec::default(),
                file_type: FileType::KvmDevice, // 文件夹，block设备，char设备
                mode: 0o666,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: make_rawdev(1, 4), // 这里用来作为device number
            },
            // fdata: InodeInfo {
            //     kvm: kvm,
            // },
        };

        let result = Arc::new(LockedVcpuInode(SpinLock::new(inode)));
        result.0.lock().self_ref = Arc::downgrade(&result);

        return result;
    }
}


impl IndexNode for LockedVcpuInode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn open(&self, _data: &mut FilePrivateData, _mode: &FileMode) -> Result<(), SystemError> {
        kdebug!("file private data:{:?}", _data);
        return Ok(())
    }

    fn close(&self, _data: &mut FilePrivateData) -> Result<(), SystemError> {
        return Ok(())
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        return Ok(self.0.lock().metadata.clone());
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.0.lock().fs.upgrade().unwrap();
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        inode.metadata.atime = metadata.atime;
        inode.metadata.mtime = metadata.mtime;
        inode.metadata.ctime = metadata.ctime;
        inode.metadata.mode = metadata.mode;
        inode.metadata.uid = metadata.uid;
        inode.metadata.gid = metadata.gid;

        return Ok(());
    }

    fn poll(&self) -> Result<PollStatus, SystemError> {
        return Ok(PollStatus::READ | PollStatus::WRITE);
    }

    /// @brief io control接口
    ///
    /// @param cmd 命令
    /// @param data 数据
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn ioctl(&self, cmd: u32, data: usize) -> Result<usize, SystemError> {
        match cmd {
            0xdeadbeef => {
                kdebug!("kvm_cpu ioctl");
                Ok(0)
            },
            _ => {
                kdebug!("kvm_cpu ioctl");
                Ok(usize::MAX)
            },
        }
    }
    /// 读设备 - 应该调用设备的函数读写，而不是通过文件系统读写
    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    /// 写设备 - 应该调用设备的函数读写，而不是通过文件系统读写
    fn write_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &[u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }
}

