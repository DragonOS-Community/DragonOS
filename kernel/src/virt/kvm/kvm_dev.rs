use crate::driver::base::device::device_number::DeviceNumber;
use crate::filesystem::devfs::{DevFS, DeviceINode};
use crate::filesystem::vfs::{
    core::generate_inode_id,
    file::{File, FileMode},
    FilePrivateData, FileSystem, FileType, IndexNode, Metadata,
};
use crate::process::ProcessManager;
use crate::{arch::KVMArch, libs::spinlock::SpinLock, time::TimeSpec};
use crate::{filesystem, kdebug};
// use crate::virt::kvm::{host_stack};
use super::push_vm;
use crate::virt::kvm::vm_dev::LockedVmInode;
use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

pub const KVM_API_VERSION: u32 = 12;

// use crate::virt::kvm::kvm_dev_ioctl_create_vm;
/*
 * ioctls for /dev/kvm fds:
 */
pub const KVM_GET_API_VERSION: u32 = 0x00;
pub const KVM_CREATE_VM: u32 = 0x01;
pub const KVM_CHECK_EXTENSION: u32 = 0x03;
pub const KVM_GET_VCPU_MMAP_SIZE: u32 = 0x04; // Get size for mmap(vcpu_fd) in bytes
pub const KVM_TRACE_ENABLE: u32 = 0x05;
pub const KVM_TRACE_PAUSE: u32 = 0x06;
pub const KVM_TRACE_DISABLE: u32 = 0x07;

#[derive(Debug)]
pub struct KvmInode {
    /// uuid 暂时不知道有什么用（x
    // uuid: Uuid,
    /// 指向自身的弱引用
    self_ref: Weak<LockedKvmInode>,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<DevFS>,
    /// INode 元数据
    metadata: Metadata,
}

#[derive(Debug)]
pub struct LockedKvmInode(SpinLock<KvmInode>);

impl LockedKvmInode {
    pub fn new() -> Arc<Self> {
        let inode = KvmInode {
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
                mode: filesystem::vfs::syscall::ModeType::S_IALLUGO,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::default(), // 这里用来作为device number
            },
        };

        let result = Arc::new(LockedKvmInode(SpinLock::new(inode)));
        result.0.lock().self_ref = Arc::downgrade(&result);

        return result;
    }
}

impl DeviceINode for LockedKvmInode {
    fn set_fs(&self, fs: Weak<DevFS>) {
        self.0.lock().fs = fs;
    }
}

impl IndexNode for LockedKvmInode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn open(&self, _data: &mut FilePrivateData, _mode: &FileMode) -> Result<(), SystemError> {
        kdebug!("file private data:{:?}", _data);
        return Ok(());
    }

    fn close(&self, _data: &mut FilePrivateData) -> Result<(), SystemError> {
        return Ok(());
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

    /// @brief io control接口
    ///
    /// @param cmd 命令
    /// @param data 数据
    ///
    /// @return 成功：Ok()
    ///         失败：Err(错误码)
    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        _private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        match cmd {
            0xdeadbeef => {
                kdebug!("kvm ioctl");
                Ok(0)
            }
            KVM_GET_API_VERSION => Ok(KVM_API_VERSION as usize),
            KVM_CREATE_VM => {
                kdebug!("kvm KVM_CREATE_VM");
                kvm_dev_ioctl_create_vm(data)
            }
            KVM_CHECK_EXTENSION
            | KVM_GET_VCPU_MMAP_SIZE
            | KVM_TRACE_ENABLE
            | KVM_TRACE_PAUSE
            | KVM_TRACE_DISABLE => Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
            _ => KVMArch::kvm_arch_dev_ioctl(cmd, data),
        }
    }
    /// 读设备 - 应该调用设备的函数读写，而不是通过文件系统读写
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    /// 写设备 - 应该调用设备的函数读写，而不是通过文件系统读写
    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }
}

#[no_mangle]
pub fn kvm_dev_ioctl_create_vm(_vmtype: usize) -> Result<usize, SystemError> {
    push_vm(0).expect("need a valid vm!");

    // 创建vm文件，返回文件描述符
    let vm_inode = LockedVmInode::new();
    let file: File = File::new(vm_inode, FileMode::O_RDWR)?;
    let r = ProcessManager::current_pcb()
        .fd_table()
        .write()
        .alloc_fd(file, None)
        .map(|fd| fd as usize);
    return r;
}
