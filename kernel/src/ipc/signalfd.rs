use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::mem::size_of;

use bitflags::bitflags;

use crate::arch::ipc::signal::{SigSet, Signal};
use crate::arch::MMArch;
use crate::filesystem::epoll::event_poll::{EventPoll, LockedEPItemLinkedList};
use crate::filesystem::epoll::{EPollEventType, EPollItem};
use crate::filesystem::vfs::file::FileFlags;
use crate::filesystem::vfs::{
    vcore::generate_inode_id, FilePrivateData, FileSystem, FileType, FsInfo, IndexNode, InodeFlags,
    InodeMode, Magic, Metadata, PollableInode, SuperBlock,
};
use crate::libs::mutex::MutexGuard;
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::libs::wait_queue::WaitQueue;
use crate::mm::MemoryManagementArch;
use crate::process::ProcessManager;
use crate::syscall::user_access::UserBufferReader;
use system_error::SystemError;

lazy_static::lazy_static! {
    static ref SIGNALFD_FS: Arc<SignalFdFs> = Arc::new(SignalFdFs);
}

#[derive(Debug)]
pub struct SignalFdFs;

impl SignalFdFs {
    pub fn instance() -> Arc<Self> {
        SIGNALFD_FS.clone()
    }
}

impl FileSystem for SignalFdFs {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        // signalfd 为伪文件系统（anon_inode 风格），root inode 不会被真正使用。
        Arc::new(SignalFdInode::new(SigSet::empty(), SignalFdFlags::empty()))
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: 255,
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "signalfd"
    }

    fn super_block(&self) -> SuperBlock {
        SuperBlock::new(
            Magic::EVENTFD_MAGIC,
            <MMArch as MemoryManagementArch>::PAGE_SIZE as u64,
            255,
        )
    }
}

bitflags! {
    pub struct SignalFdFlags: u32 {
        const SFD_NONBLOCK = 0o0004000;
        const SFD_CLOEXEC  = 0o02000000;
    }
}

/// Linux signalfd_siginfo 为 128 字节。
/// gVisor 测试仅检查 read 返回值为 sizeof(signalfd_siginfo)。
#[repr(C)]
#[derive(Clone, Copy)]
struct SignalFdSigInfo {
    bytes: [u8; 128],
}

impl SignalFdSigInfo {
    fn from_signal(sig: Signal) -> Self {
        let mut bytes = [0u8; 128];
        // ssi_signo (u32)
        bytes[0..4].copy_from_slice(&(sig as u32).to_ne_bytes());
        Self { bytes }
    }
}

#[derive(Debug)]
struct SignalFdState {
    mask: SigSet,
    flags: SignalFdFlags,
    metadata: Metadata,
}

#[derive(Debug)]
pub struct SignalFdInode {
    state: SpinLock<SignalFdState>,
    wait_queue: WaitQueue,
    epitems: LockedEPItemLinkedList,
}

impl SignalFdInode {
    pub fn new(mask: SigSet, flags: SignalFdFlags) -> Self {
        let metadata = Metadata {
            dev_id: 0,
            inode_id: generate_inode_id(),
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: crate::time::PosixTimeSpec::default(),
            mtime: crate::time::PosixTimeSpec::default(),
            ctime: crate::time::PosixTimeSpec::default(),
            btime: crate::time::PosixTimeSpec::default(),
            file_type: FileType::CharDevice,
            mode: InodeMode::from_bits_truncate(0o600),
            nlinks: 1,
            uid: 0,
            gid: 0,
            raw_dev: Default::default(),
            flags: InodeFlags::empty(),
        };
        Self {
            state: SpinLock::new(SignalFdState {
                mask,
                flags,
                metadata,
            }),
            wait_queue: WaitQueue::default(),
            epitems: LockedEPItemLinkedList::default(),
        }
    }

    fn has_pending_for_mask(&self, mask: &SigSet) -> bool {
        let pcb = ProcessManager::current_pcb();
        let siginfo = pcb.sig_info_irqsave();
        let mut pending = siginfo.sig_pending().signal();
        drop(siginfo);
        pending |= pcb.sighand().shared_pending_signal();
        !(pending & *mask).is_empty()
    }

    fn readable(&self) -> bool {
        let mask = self.state.lock().mask;
        self.has_pending_for_mask(&mask)
    }

    pub fn notify_signal(&self, sig: Signal) {
        let mask = self.state.lock().mask;
        if !mask.contains(sig.into()) {
            return;
        }
        // 唤醒阻塞 read() 的等待者
        self.wait_queue.wakeup_all(None);
        // 唤醒 epoll 等待者
        let _ = EventPoll::wakeup_epoll(
            &self.epitems,
            EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
        );
    }

    fn dequeue_one(&self) -> Option<Signal> {
        let pcb = ProcessManager::current_pcb();
        let mut siginfo = pcb.sig_info_mut();
        let mask = self.state.lock().mask;
        let ignore_mask = mask.complement();
        let (sig, _info) = siginfo.dequeue_signal(&ignore_mask, &pcb);
        drop(siginfo);
        if sig == Signal::INVALID {
            None
        } else {
            Some(sig)
        }
    }

    fn nonblock(&self, state_guard: &SpinLockGuard<'_, SignalFdState>) -> bool {
        state_guard.flags.contains(SignalFdFlags::SFD_NONBLOCK)
    }

    pub(super) fn set_mask_and_flags(&self, mask: SigSet, flags: SignalFdFlags) {
        let mut guard = self.state.lock();
        guard.mask = mask;
        guard.flags = flags;
    }
}

impl PollableInode for SignalFdInode {
    fn poll(&self, _private_data: &FilePrivateData) -> Result<usize, SystemError> {
        let mask = self.state.lock().mask;
        if self.has_pending_for_mask(&mask) {
            Ok((EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM).bits() as usize)
        } else {
            Ok(0)
        }
    }

    fn add_epitem(
        &self,
        epitem: Arc<EPollItem>,
        _private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        self.epitems.lock().push_back(epitem);
        Ok(())
    }

    fn remove_epitem(
        &self,
        epitem: &Arc<EPollItem>,
        _private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let mut guard = self.epitems.lock();
        let len = guard.len();
        guard.retain(|x| !Arc::ptr_eq(x, epitem));
        if guard.len() != len {
            Ok(())
        } else {
            Err(SystemError::ENOENT)
        }
    }
}

impl IndexNode for SignalFdInode {
    fn is_stream(&self) -> bool {
        true
    }

    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        data_guard: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // 释放 FilePrivateData 锁，避免在阻塞时持有锁导致 panic
        drop(data_guard);

        if len < size_of::<SignalFdSigInfo>() {
            return Err(SystemError::EINVAL);
        }

        loop {
            if let Some(sig) = self.dequeue_one() {
                let info = SignalFdSigInfo::from_signal(sig);
                buf[0..size_of::<SignalFdSigInfo>()].copy_from_slice(&info.bytes);
                return Ok(size_of::<SignalFdSigInfo>());
            }

            let state_guard = self.state.lock();
            if self.nonblock(&state_guard) {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            drop(state_guard);

            // 阻塞等待：由 notify_signal() 唤醒
            let r = wq_wait_event_interruptible!(self.wait_queue, self.readable(), {});
            if r.is_err() {
                return Err(SystemError::ERESTARTSYS);
            }
        }
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EINVAL)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        SignalFdFs::instance()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        Err(SystemError::EINVAL)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.state.lock().metadata.clone())
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Ok(self)
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(String::from("signalfd"))
    }
}

pub(super) fn read_user_sigset(mask_ptr: usize, mask_size: usize) -> Result<SigSet, SystemError> {
    // Linux 内核 ABI 中，rt_sig* / signalfd* 的 sigsetsize 通常为 8（64-bit）。
    // 但用户态 libc 可能传入更大的 sigset_t（例如 1024-bit / 128 bytes）。
    // 我们当前只支持 64 个信号，因此只读取前 8 字节。
    if mask_size < size_of::<u64>() {
        return Err(SystemError::EINVAL);
    }
    let reader = UserBufferReader::new(mask_ptr as *const u64, size_of::<u64>(), true)?;
    let bits = *reader.read_one_from_user::<u64>(0)?;
    Ok(SigSet::from_bits_truncate(bits))
}

/// 在向 pcb 投递信号后，唤醒该 pcb 中所有匹配 mask 的 signalfd。
pub fn notify_signalfd_for_pcb(pcb: &Arc<crate::process::ProcessControlBlock>, sig: Signal) {
    let fd_table = {
        let basic = pcb.basic();
        basic.try_fd_table()
    };
    let Some(fd_table) = fd_table else {
        // 该任务可能正在退出或是内核线程，没有 fd_table。
        return;
    };
    let guard = fd_table.read();
    for (_fd, file) in guard.iter() {
        let inode = file.inode();
        if let Some(sfd) = inode.as_any_ref().downcast_ref::<SignalFdInode>() {
            sfd.notify_signal(sig);
        }
    }
}
