use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use system_error::SystemError;

use crate::{filesystem::vfs::file::FileFlags, libs::wait_queue::WaitQueue};

use super::{conn::FuseConn, inode::FuseNode};

#[derive(Debug, Clone)]
pub struct FuseDevPrivateData {
    pub conn: Arc<dyn Any + Send + Sync>,
    pub nonblock: bool,
}

impl FuseDevPrivateData {
    pub fn conn_ref(&self) -> Result<Arc<FuseConn>, SystemError> {
        downcast_conn(&self.conn)
    }
}

#[derive(Debug, Clone)]
pub struct FuseOpenPrivateData {
    pub conn: Arc<dyn Any + Send + Sync>,
    pub node: Arc<FuseNode>,
    pub fh: u64,
    /// User-visible file flags sent in FUSE_OPEN/FUSE_READ/FUSE_WRITE.
    pub open_flags: u32,
    /// Daemon-returned FOPEN_* flags from fuse_open_out.
    pub fopen_flags: u32,
    pub no_open: bool,
    pub writeback_handle: Option<Arc<FuseWritebackHandle>>,
}

pub struct FuseWritebackHandle {
    pub fh: u64,
    open_flags: AtomicU32,
    pub fopen_flags: u32,
    pub no_open: bool,
    closing: AtomicBool,
    inflight: AtomicUsize,
    wait_queue: WaitQueue,
}

impl FuseOpenPrivateData {
    pub fn set_open_flags(&mut self, flags: FileFlags) {
        let bits = flags.bits();
        self.open_flags = bits;
        if let Some(handle) = &self.writeback_handle {
            handle.set_open_flags(bits);
        }
    }
}

impl FuseFilePrivateData {
    pub fn set_flags(&mut self, flags: FileFlags) {
        match self {
            FuseFilePrivateData::File(p) | FuseFilePrivateData::Dir(p) => p.set_open_flags(flags),
            FuseFilePrivateData::Dev(p) => {
                p.nonblock = flags.contains(FileFlags::O_NONBLOCK);
            }
        }
    }
}

impl core::fmt::Debug for FuseWritebackHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FuseWritebackHandle")
            .field("fh", &self.fh)
            .field("open_flags", &self.open_flags())
            .field("fopen_flags", &self.fopen_flags)
            .field("no_open", &self.no_open)
            .field("closing", &self.is_closing())
            .field("inflight", &self.inflight.load(Ordering::Acquire))
            .finish()
    }
}

impl FuseWritebackHandle {
    pub fn new(fh: u64, open_flags: u32, fopen_flags: u32, no_open: bool) -> Self {
        Self {
            fh,
            open_flags: AtomicU32::new(open_flags),
            fopen_flags,
            no_open,
            closing: AtomicBool::new(false),
            inflight: AtomicUsize::new(0),
            wait_queue: WaitQueue::default(),
        }
    }

    pub fn is_writable(&self) -> bool {
        let flags = FileFlags::from_bits_truncate(self.open_flags());
        let access = flags.access_flags();
        access == FileFlags::O_WRONLY || access == FileFlags::O_RDWR
    }

    pub fn open_flags(&self) -> u32 {
        self.open_flags.load(Ordering::Acquire)
    }

    pub fn set_open_flags(&self, open_flags: u32) {
        self.open_flags.store(open_flags, Ordering::Release);
    }

    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    pub fn mark_closing(&self) {
        self.closing.store(true, Ordering::Release);
    }

    pub fn wait_inflight_zero(&self) {
        self.wait_queue.wait_until(|| {
            if self.inflight.load(Ordering::Acquire) == 0 {
                Some(())
            } else {
                None
            }
        });
    }

    fn unpin_inflight(&self) {
        if self.inflight.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.wait_queue.wake_all();
        }
    }

    pub fn try_pin(self: &Arc<Self>) -> Option<FuseWritebackHandlePin> {
        if self.is_closing() || !self.is_writable() {
            return None;
        }
        self.inflight.fetch_add(1, Ordering::AcqRel);
        if self.is_closing() {
            self.unpin_inflight();
            return None;
        }
        Some(FuseWritebackHandlePin {
            handle: self.clone(),
        })
    }
}

pub struct FuseWritebackHandlePin {
    handle: Arc<FuseWritebackHandle>,
}

impl FuseWritebackHandlePin {
    pub fn handle(&self) -> &FuseWritebackHandle {
        &self.handle
    }
}

impl Drop for FuseWritebackHandlePin {
    fn drop(&mut self) {
        self.handle.unpin_inflight();
    }
}

#[derive(Debug, Clone)]
pub enum FuseFilePrivateData {
    Dev(FuseDevPrivateData),
    File(FuseOpenPrivateData),
    Dir(FuseOpenPrivateData),
}

#[inline]
fn downcast_conn(conn_any: &Arc<dyn Any + Send + Sync>) -> Result<Arc<FuseConn>, SystemError> {
    conn_any
        .clone()
        .downcast::<FuseConn>()
        .map_err(|_| SystemError::EINVAL)
}
