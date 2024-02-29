use crate::driver::base::device::device_number::DeviceNumber;
use crate::filesystem::devfs::DevFS;
use crate::filesystem::vfs::{
    core::generate_inode_id,
    file::{File, FileMode},
    FilePrivateData, FileSystem, FileType, IndexNode, Metadata,
};
use crate::mm::VirtAddr;
use crate::process::ProcessManager;
use crate::syscall::user_access::copy_from_user;
use crate::virt::kvm::host_mem::KvmUserspaceMemoryRegion;
use crate::virt::kvm::update_vm;
use crate::virt::kvm::vcpu_dev::LockedVcpuInode;
use crate::virt::kvm::vm;
use crate::{arch::KVMArch, libs::spinlock::SpinLock, time::TimeSpec};
use crate::{filesystem, kdebug};
use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

// pub const KVM_API_VERSION:u32 = 12;
// pub const GUEST_STACK_SIZE:usize = 1024;
// pub const HOST_STACK_SIZE:usize = 0x1000 * 6;

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
pub struct VmInode {
    /// uuid 暂时不知道有什么用（x
    // uuid: Uuid,
    /// 指向自身的弱引用
    self_ref: Weak<LockedVmInode>,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<DevFS>,
    /// INode 元数据
    metadata: Metadata,
    // fdata: InodeInfo,
}

#[derive(Debug)]
pub struct LockedVmInode(SpinLock<VmInode>);

impl LockedVmInode {
    pub fn new() -> Arc<Self> {
        let inode = VmInode {
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
            // fdata: InodeInfo {
            //     kvm: kvm,
            // },
        };

        let result = Arc::new(LockedVmInode(SpinLock::new(inode)));
        result.0.lock().self_ref = Arc::downgrade(&result);

        return result;
    }
}

impl IndexNode for LockedVmInode {
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
                kdebug!("kvm_vm ioctl");
                Ok(0)
            }
            KVM_CREATE_VCPU => {
                kdebug!("kvm_vcpu ioctl KVM_CREATE_VCPU");
                kvm_vm_ioctl_create_vcpu(data as u32)
            }
            KVM_SET_USER_MEMORY_REGION => {
                kdebug!("kvm_vcpu ioctl KVM_SET_USER_MEMORY_REGION data={:x}", data);
                let mut kvm_userspace_mem = KvmUserspaceMemoryRegion::default(); // = unsafe { (data as *const KvmUserspaceMemoryRegion).as_ref().unwrap() };
                unsafe {
                    copy_from_user(
                        core::slice::from_raw_parts_mut(
                            (&mut kvm_userspace_mem as *mut _) as *mut u8,
                            core::mem::size_of::<KvmUserspaceMemoryRegion>(),
                        ),
                        VirtAddr::new(data),
                    )?;
                }
                kdebug!(
                    "slot={}, flag={}, memory_size={:x}, guest_phys_addr={}, userspace_addr={}",
                    kvm_userspace_mem.slot,
                    kvm_userspace_mem.flags,
                    kvm_userspace_mem.memory_size,
                    kvm_userspace_mem.guest_phys_addr, // starting at physical address guest_phys_addr (from the guest’s perspective)
                    kvm_userspace_mem.userspace_addr // using memory at linear address userspace_addr (from the host’s perspective)
                );

                let mut current_vm = vm(0).unwrap();
                current_vm.set_user_memory_region(&kvm_userspace_mem)?;
                update_vm(0, current_vm);
                Ok(0)
            }
            KVM_GET_DIRTY_LOG | KVM_IRQFD | KVM_IOEVENTFD | KVM_IRQ_LINE_STATUS => {
                Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
            }
            _ => {
                kdebug!("kvm_vm ioctl");
                Ok(usize::MAX)
            }
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

fn kvm_vm_ioctl_create_vcpu(id: u32) -> Result<usize, SystemError> {
    let vcpu = KVMArch::kvm_arch_vcpu_create(id).unwrap();
    KVMArch::kvm_arch_vcpu_setup(vcpu.as_ref())?;

    let mut current_vm = vm(0).unwrap();
    current_vm.vcpu.push(vcpu);
    current_vm.nr_vcpus += 1;
    update_vm(0, current_vm);

    let vcpu_inode = LockedVcpuInode::new();
    let file: File = File::new(vcpu_inode, FileMode::O_RDWR)?;
    let r = ProcessManager::current_pcb()
        .fd_table()
        .write()
        .alloc_fd(file, None)
        .map(|fd| fd as usize);
    return r;
}
