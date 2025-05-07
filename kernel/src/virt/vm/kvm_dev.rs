use core::intrinsics::unlikely;

use alloc::sync::{Arc, Weak};
use log::{debug, warn};
use system_error::SystemError;

use crate::{
    arch::{
        vm::{kvm_host::KvmCommonRegs, uapi::UapiKvmSegmentRegs},
        MMArch,
    },
    driver::base::device::device_number::DeviceNumber,
    filesystem::{
        devfs::{devfs_register, DevFS, DeviceINode},
        vfs::{
            file::{File, FileMode},
            syscall::ModeType,
            vcore::generate_inode_id,
            FileType, IndexNode, Metadata,
        },
    },
    libs::spinlock::SpinLock,
    mm::MemoryManagementArch,
    process::ProcessManager,
    syscall::user_access::{UserBufferReader, UserBufferWriter},
    time::PosixTimeSpec,
    virt::vm::user_api::{KvmUserspaceMemoryRegion, PosixKvmUserspaceMemoryRegion},
};

use super::kvm_host::{vcpu::LockedVirtCpu, LockedVm};

#[derive(Debug)]
pub struct KvmInode {
    /// 指向自身的弱引用
    self_ref: Weak<LockedKvmInode>,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<DevFS>,
    /// INode 元数据
    metadata: Metadata,
}

#[derive(Debug)]
pub struct LockedKvmInode {
    inner: SpinLock<KvmInode>,
}

impl LockedKvmInode {
    const KVM_CREATE_VM: u32 = 0xAE01;
    const KVM_GET_VCPU_MMAP_SIZE: u32 = 0xAE04;

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
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                btime: PosixTimeSpec::default(),
                file_type: FileType::KvmDevice, // 文件夹，block设备，char设备
                mode: ModeType::S_IALLUGO,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::default(), // 这里用来作为device number
            },
        };

        let result = Arc::new(LockedKvmInode {
            inner: SpinLock::new(inode),
        });
        result.inner.lock().self_ref = Arc::downgrade(&result);

        return result;
    }

    fn create_vm(&self, vm_type: usize) -> Result<usize, SystemError> {
        let kvm = LockedVm::create(vm_type)?;

        let instance = KvmInstance::new(kvm);

        let current = ProcessManager::current_pcb();

        let file = File::new(instance, FileMode::O_RDWR)?;
        let fd = current.fd_table().write().alloc_fd(file, None)?;
        return Ok(fd as usize);
    }
}

impl DeviceINode for LockedKvmInode {
    fn set_fs(&self, fs: Weak<DevFS>) {
        self.inner.lock().fs = fs;
    }
}

impl IndexNode for LockedKvmInode {
    fn open(
        &self,
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
        _mode: &FileMode,
    ) -> Result<(), SystemError> {
        Ok(())
    }
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, system_error::SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, system_error::SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        self.inner.lock().fs.upgrade().unwrap()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, system_error::SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn metadata(&self) -> Result<Metadata, system_error::SystemError> {
        Ok(self.inner.lock().metadata.clone())
    }

    fn ioctl(
        &self,
        cmd: u32,
        arg: usize,
        _private_data: &crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, SystemError> {
        match cmd {
            Self::KVM_CREATE_VM => {
                let ret = self.create_vm(arg);
                warn!("[KVM]: KVM_CREATE_VM {ret:?}");

                return ret;
            }

            Self::KVM_GET_VCPU_MMAP_SIZE => {
                if arg != 0 {
                    return Err(SystemError::EINVAL);
                }
                debug!("[KVM] KVM_GET_VCPU_MMAP_SIZE");
                return Ok(MMArch::PAGE_SIZE);
            }

            _ => {
                // TODO: arch_ioctl
                warn!("[KVM]: unknown iooctl cmd {cmd:x}");
            }
        }

        Ok(0)
    }

    fn close(
        &self,
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<(), SystemError> {
        Ok(())
    }
}

#[derive(Debug)]
pub struct KvmInstance {
    kvm: Arc<LockedVm>,
    metadata: Metadata,
}

impl KvmInstance {
    const KVM_CREATE_VCPU: u32 = 0xAE41;
    const KVM_SET_USER_MEMORY_REGION: u32 = 0x4020AE46;

    pub fn new(vm: Arc<LockedVm>) -> Arc<Self> {
        Arc::new(Self {
            kvm: vm,
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
                file_type: FileType::KvmDevice,
                mode: ModeType::S_IALLUGO,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::default(), // 这里用来作为device number
            },
        })
    }
}

impl IndexNode for KvmInstance {
    fn open(
        &self,
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
        _mode: &crate::filesystem::vfs::file::FileMode,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    #[inline(never)]
    fn ioctl(
        &self,
        cmd: u32,
        arg: usize,
        _private_data: &crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, SystemError> {
        debug!("kvm instance ioctl cmd {cmd:x}");
        match cmd {
            Self::KVM_CREATE_VCPU => {
                let ret = self.kvm.lock().create_vcpu(arg);
                debug!("[KVM] create vcpu fd {ret:?}");
                return ret;
            }

            Self::KVM_SET_USER_MEMORY_REGION => {
                debug!("[KVM-INSTANCE] KVM_SET_USER_MEMORY_REGION");
                let user_reader = UserBufferReader::new(
                    arg as *const PosixKvmUserspaceMemoryRegion,
                    core::mem::size_of::<PosixKvmUserspaceMemoryRegion>(),
                    true,
                )?;

                let region = user_reader.read_one_from_user::<PosixKvmUserspaceMemoryRegion>(0)?;

                self.kvm
                    .lock()
                    .set_memory_region(KvmUserspaceMemoryRegion::from_posix(region)?)?;

                return Ok(0);
            }

            _ => {
                // arch_ioctl
            }
        }

        todo!()
    }

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        todo!()
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        todo!()
    }

    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        todo!()
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, SystemError> {
        todo!()
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.metadata.clone())
    }

    fn close(
        &self,
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<(), SystemError> {
        Ok(())
    }
}

#[derive(Debug)]
pub struct KvmVcpuDev {
    vcpu: Arc<LockedVirtCpu>,
    /// INode 元数据
    metadata: Metadata,
}

impl KvmVcpuDev {
    const KVM_RUN: u32 = 0xAE80;
    const KVM_GET_REGS: u32 = 0x8090AE81;
    const KVM_SET_REGS: u32 = 0x4090AE82;
    const KVM_GET_SREGS: u32 = 0x8138AE83;
    const KVM_SET_SREGS: u32 = 0x4138AE84;

    pub fn new(vcpu: Arc<LockedVirtCpu>) -> Arc<Self> {
        Arc::new(Self {
            vcpu,
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
                file_type: FileType::KvmDevice, // 文件夹，block设备，char设备
                mode: ModeType::S_IALLUGO,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::default(), // 这里用来作为device number
            },
        })
    }
}

impl IndexNode for KvmVcpuDev {
    fn open(
        &self,
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
        _mode: &FileMode,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn close(
        &self,
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn ioctl(
        &self,
        cmd: u32,
        arg: usize,
        _private_data: &crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, SystemError> {
        match cmd {
            Self::KVM_RUN => {
                if arg != 0 {
                    return Err(SystemError::EINVAL);
                }
                let mut vcpu = self.vcpu.lock();
                let oldpid = vcpu.pid;
                if unlikely(oldpid != Some(ProcessManager::current_pid())) {
                    vcpu.pid = Some(ProcessManager::current_pid());
                }

                return vcpu.run();
            }
            Self::KVM_GET_REGS => {
                let kvm_regs = self.vcpu.lock().get_regs();
                let mut user_writer = UserBufferWriter::new(
                    arg as *const KvmCommonRegs as *mut KvmCommonRegs,
                    core::mem::size_of::<KvmCommonRegs>(),
                    true,
                )?;

                user_writer.copy_one_to_user(&kvm_regs, 0)?;
                return Ok(0);
            }

            Self::KVM_SET_REGS => {
                let user_reader = UserBufferReader::new(
                    arg as *const KvmCommonRegs,
                    core::mem::size_of::<KvmCommonRegs>(),
                    true,
                )?;

                let regs = user_reader.read_one_from_user::<KvmCommonRegs>(0)?;

                self.vcpu.lock().set_regs(regs)?;

                return Ok(0);
            }

            Self::KVM_GET_SREGS => {
                let sregs = self.vcpu.lock().get_segment_regs();

                let mut writer = UserBufferWriter::new(
                    arg as *const UapiKvmSegmentRegs as *mut UapiKvmSegmentRegs,
                    core::mem::size_of::<UapiKvmSegmentRegs>(),
                    true,
                )?;

                writer.copy_one_to_user(&sregs, 0)?;

                return Ok(0);
            }

            Self::KVM_SET_SREGS => {
                let user_reader = UserBufferReader::new(
                    arg as *const UapiKvmSegmentRegs,
                    core::mem::size_of::<UapiKvmSegmentRegs>(),
                    true,
                )?;

                let mut sreg = UapiKvmSegmentRegs::default();
                user_reader.copy_one_from_user(&mut sreg, 0)?;

                if let Ok(_res) = self.vcpu.lock().set_segment_regs(&mut sreg) {
                    return Ok(0);
                } else {
                    debug!("set segment regs failed");
                    return Err(SystemError::EINVAL);
                }
            }

            _ => {
                // arch ioctl
                warn!("[KVM-VCPU] unknown ioctl cmd {cmd:x}");
            }
        }

        Ok(0)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.metadata.clone())
    }

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        todo!()
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        todo!()
    }

    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        todo!()
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, SystemError> {
        todo!()
    }
}

pub fn kvm_init() -> Result<(), SystemError> {
    let kvm_inode = LockedKvmInode::new();

    devfs_register("kvm", kvm_inode)?;

    Ok(())
}
