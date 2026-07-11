use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use system_error::SystemError;

use crate::{
    filesystem::vfs::file::FileFlags,
    libs::{errseq::ErrSeqValue, mutex::Mutex, wait_queue::WaitQueue},
    mm::readahead::FileReadaheadState,
};

use super::{
    conn::{FuseConn, FuseRequestCred},
    inode::FuseNode,
};

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
    pub open_context: FuseOpenContext,
    pub writeback_handle: Option<Arc<FuseWritebackHandle>>,
    /// Per-open-file-description state. Cloning private data (dup/snapshots)
    /// keeps one shared sequential-read history.
    pub readahead_state: Arc<Mutex<FileReadaheadState>>,
    pub lifetime: Arc<FuseOpenLifetime>,
}

#[derive(Debug)]
pub struct FuseOpenLifetime {
    closing: AtomicBool,
    inflight: AtomicUsize,
    wait_queue: WaitQueue,
}

impl FuseOpenLifetime {
    pub fn new() -> Self {
        Self {
            closing: AtomicBool::new(false),
            inflight: AtomicUsize::new(0),
            wait_queue: WaitQueue::default(),
        }
    }

    pub fn try_pin(self: &Arc<Self>) -> Option<FuseOpenLifetimePin> {
        if self.closing.load(Ordering::Acquire) {
            return None;
        }
        self.inflight.fetch_add(1, Ordering::AcqRel);
        if self.closing.load(Ordering::Acquire) {
            self.unpin();
            return None;
        }
        Some(FuseOpenLifetimePin(self.clone()))
    }

    pub fn close_and_wait(&self) {
        self.closing.store(true, Ordering::Release);
        self.wait_queue.wait_until(|| {
            (self.inflight.load(Ordering::Acquire) == 0).then_some(())
        });
    }

    fn unpin(&self) {
        if self.inflight.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.wait_queue.wake_all();
        }
    }
}

#[derive(Debug)]
pub struct FuseOpenLifetimePin(Arc<FuseOpenLifetime>);

impl Drop for FuseOpenLifetimePin {
    fn drop(&mut self) {
        self.0.unpin();
    }
}

#[derive(Debug, Clone)]
pub struct FuseOpenContext {
    /// Request credential sampled at open time for asynchronous writeback.
    pub request_cred: FuseRequestCred,
    /// POSIX lock-owner id for requests whose semantics depend on the opener.
    pub lock_owner: u64,
    /// Node generation observed when this file was opened.
    pub node_generation: u64,
    /// Per-open-file writeback error cursor used by FUSE file-level sync paths.
    pub wb_errseq: Arc<Mutex<ErrSeqValue>>,
}

pub struct FuseWritebackHandle {
    pub fh: u64,
    open_flags: AtomicU32,
    pub fopen_flags: u32,
    pub no_open: bool,
    pub open_context: FuseOpenContext,
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
            .field("open_context", &self.open_context)
            .field("closing", &self.is_closing())
            .field("inflight", &self.inflight.load(Ordering::Acquire))
            .finish()
    }
}

impl FuseWritebackHandle {
    pub fn new(
        fh: u64,
        open_flags: u32,
        fopen_flags: u32,
        no_open: bool,
        open_context: FuseOpenContext,
    ) -> Self {
        Self {
            fh,
            open_flags: AtomicU32::new(open_flags),
            fopen_flags,
            no_open,
            open_context,
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
