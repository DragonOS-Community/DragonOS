use super::{DevFS, DeviceINode, LockedDevFSInode};
use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::vfs::{
        file::FileFlags, utils::DName, vcore::generate_inode_id, FilePrivateData, FileSystem,
        FileType, IndexNode, InodeFlags, InodeMode, Metadata,
    },
    filesystem::{
        epoll::{EPollEventType, EPollItem},
        vfs::{PollableInode, UserReadMode},
    },
    libs::{
        mutex::{Mutex, MutexGuard},
        rand::{
            credit_random_bits, mix_random_bytes, random_init_bits, random_is_ready,
            read_random_output, reseed_random, RANDOM_EPITEMS,
        },
    },
    process::cred::{capable, CAPFlags},
    syscall::user_buffer::UserBuffer,
    time::PosixTimeSpec,
};
use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};

use system_error::SystemError;

#[derive(Debug)]
pub struct RandomInode {
    self_ref: Weak<LockedRandomInode>,
    fs: Weak<DevFS>,
    parent: Weak<LockedDevFSInode>,
    dname: DName,
    metadata: Metadata,
    blocking: bool,
}

#[derive(Debug)]
pub struct LockedRandomInode(Mutex<RandomInode>);

impl LockedRandomInode {
    pub fn new(name: &str, raw_dev: DeviceNumber) -> Arc<Self> {
        let inode = RandomInode {
            self_ref: Weak::default(),
            fs: Weak::default(),
            parent: Weak::default(),
            dname: DName::from(name),
            blocking: name == "random",
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
                raw_dev,
            },
        };

        let result = Arc::new(LockedRandomInode(Mutex::new(inode)));
        result.0.lock().self_ref = Arc::downgrade(&result);
        result
    }
}

impl DeviceINode for LockedRandomInode {
    fn set_fs(&self, fs: Weak<DevFS>) {
        self.0.lock().fs = fs;
    }

    fn set_parent(&self, parent: Weak<LockedDevFSInode>) {
        self.0.lock().parent = parent;
    }
}

impl IndexNode for LockedRandomInode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn open(
        &self,
        mut data: MutexGuard<FilePrivateData>,
        flags: &FileFlags,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        *data = FilePrivateData::Random(*flags);
        Ok(())
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.0.lock().metadata.clone())
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.0.lock().fs.upgrade().unwrap()
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn set_metadata(
        &self,
        metadata: &Metadata,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> = {
            let mut inode = self.0.lock();
            inode.metadata.atime = metadata.atime;
            inode.metadata.mtime = metadata.mtime;
            inode.metadata.ctime = metadata.ctime;
            inode.metadata.btime = metadata.btime;
            inode.metadata.mode = metadata.mode;
            inode.metadata.uid = metadata.uid;
            inode.metadata.gid = metadata.gid;
            Ok(())
        };
        result.map_err(Into::into)
    }

    fn update_atime(&self, now: PosixTimeSpec, relatime: bool) -> Result<(), SystemError> {
        let mut inode = self.0.lock();
        crate::filesystem::vfs::update_atime_locked(&mut inode.metadata, now, relatime);
        Ok(())
    }

    fn mmap(&self, _start: usize, _len: usize, _offset: usize) -> Result<(), SystemError> {
        Err(SystemError::ENODEV)
    }

    fn has_noop_llseek(&self) -> bool {
        true
    }

    fn user_read_mode(&self) -> UserReadMode {
        UserReadMode::Iterator
    }

    fn read_user_at(
        &self,
        _offset: usize,
        len: usize,
        writer: &mut UserBuffer<'_>,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<Option<usize>, SystemError> {
        let nonblocking = matches!(&*data, FilePrivateData::Random(flags)
            if flags.contains(FileFlags::O_NONBLOCK));
        drop(data);
        let insecure = !self.0.lock().blocking;
        read_random_output(len, insecure, nonblocking, |offset, bytes| {
            writer.write_prefix_to_user(offset, bytes)
        })
        .map(Some)
    }

    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let nonblocking = matches!(&*data, FilePrivateData::Random(flags)
            if flags.contains(FileFlags::O_NONBLOCK));
        drop(data);
        if len > buf.len() {
            return Err(SystemError::EINVAL.into());
        }
        let insecure = !self.0.lock().blocking;
        read_random_output(len, insecure, nonblocking, |offset, bytes| {
            buf[offset..offset + bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        })
        .map_err(Into::into)
    }

    fn write_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        drop(data);
        if len > buf.len() {
            return Err(SystemError::EINVAL.into());
        }
        mix_random_bytes(&buf[..len]);
        Ok(len)
    }

    fn ioctl(
        &self,
        cmd: u32,
        argument: usize,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        drop(data);
        random_ioctl(cmd, argument).map_err(Into::into)
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Ok(self)
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        let parent = self.0.lock().parent.upgrade();
        if let Some(parent) = parent {
            return Ok(parent);
        }
        Err(SystemError::ENOENT)
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(self.0.lock().dname.clone())
    }
}

/// Fixed-width ioctl fields use address-only, exception-table-protected copy.
/// Never construct a Rust typed reference to an untrusted userspace address.
fn read_ioctl_i32(address: usize) -> Result<i32, SystemError> {
    let mut bytes = [0u8; 4];
    UserBuffer::new_protected(address as *mut u8, bytes.len(), true)?
        .read_from_user(0, &mut bytes)?;
    Ok(i32::from_ne_bytes(bytes))
}

// Linux uapi/linux/random.h: all count/length fields are i32, including on LP64.
fn random_ioctl(command: u32, argument: usize) -> Result<usize, SystemError> {
    const RNDGETENTCNT: u32 = 0x80045200;
    const RNDADDTOENTCNT: u32 = 0x40045201;
    const RNDADDENTROPY: u32 = 0x40085203;
    const RNDZAPENTCNT: u32 = 0x5204;
    const RNDCLEARPOOL: u32 = 0x5206;
    const RNDRESEEDCRNG: u32 = 0x5207;
    if command == RNDGETENTCNT {
        UserBuffer::new_protected(argument as *mut u8, 4, true)?
            .write_to_user(0, &(random_init_bits() as i32).to_ne_bytes())?;
        return Ok(0);
    }
    match command {
        RNDADDTOENTCNT | RNDADDENTROPY | RNDZAPENTCNT | RNDCLEARPOOL | RNDRESEEDCRNG => {
            if !capable(CAPFlags::CAP_SYS_ADMIN) {
                return Err(SystemError::EPERM);
            }
        }
        _ => return Err(SystemError::EINVAL),
    }
    match command {
        RNDADDTOENTCNT => {
            let count = read_ioctl_i32(argument)?;
            if count < 0 {
                return Err(SystemError::EINVAL);
            }
            credit_random_bits(count as u32);
        }
        RNDADDENTROPY => {
            let entropy = read_ioctl_i32(argument)?;
            if entropy < 0 {
                return Err(SystemError::EINVAL);
            }
            let length_argument = argument.checked_add(4).ok_or(SystemError::EFAULT)?;
            let byte_count = read_ioctl_i32(length_argument)?;
            // Linux converts this signed field to size_t for import, but can
            // never consume the original negative length completely.
            if byte_count < 0 {
                return Err(SystemError::EFAULT);
            }
            let input = argument.checked_add(8).ok_or(SystemError::EFAULT)?;
            let length = byte_count as usize;
            let reader = UserBuffer::new_protected(input as *mut u8, length, true)?;
            let mut block = zeroize::Zeroizing::new([0u8; kernel_rng::OUTPUT_CHUNK]);
            let mut offset = 0;
            while offset < length {
                let count = (length - offset).min(block.len());
                reader.read_from_user(offset, &mut block[..count])?;
                mix_random_bytes(&block[..count]);
                offset += count;
                if offset < length
                    && offset % 4096 == 0
                    && crate::process::ProcessManager::current_pcb().has_pending_signal_fast()
                {
                    return Err(SystemError::EFAULT);
                }
            }
            // Faults may have mixed a prefix, but must never credit it as if
            // the privileged caller's entire claimed payload was consumed.
            credit_random_bits(entropy as u32);
        }
        RNDRESEEDCRNG => reseed_random()?,
        // Linux 6.6 retained these privileged commands without clearing state.
        RNDZAPENTCNT | RNDCLEARPOOL => {}
        _ => unreachable!(),
    }
    Ok(0)
}

impl PollableInode for LockedRandomInode {
    fn poll(&self, _data: &FilePrivateData) -> Result<usize, SystemError> {
        let events = if !self.0.lock().blocking {
            EPollEventType::EPOLLIN
                | EPollEventType::EPOLLRDNORM
                | EPollEventType::EPOLLOUT
                | EPollEventType::EPOLLWRNORM
        } else if random_is_ready() {
            EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM
        } else {
            EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM
        };
        Ok(events.bits() as usize)
    }

    fn add_epitem(&self, item: Arc<EPollItem>, _data: &FilePrivateData) -> Result<(), SystemError> {
        RANDOM_EPITEMS.add(item);
        Ok(())
    }

    fn remove_epitem(
        &self,
        item: &Arc<EPollItem>,
        _data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        RANDOM_EPITEMS.remove(item)
    }
}
