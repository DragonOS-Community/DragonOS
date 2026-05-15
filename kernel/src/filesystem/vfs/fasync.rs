//! Asynchronous I/O notification (SIGIO/SIGURG) support
//!
//! This module provides the fasync mechanism for sending SIGIO signals
//! to processes when I/O events occur on file descriptors.
//!
//! In Linux, this is a general mechanism used by various file types
//! including sockets, pipes, ttys, etc.

use alloc::{sync::Weak, vec::Vec};
use core::sync::atomic::compiler_fence;

use crate::{
    arch::ipc::signal::Signal,
    filesystem::epoll::EPollEventType,
    ipc::signal_types::{SigCode, SigInfo, SigType, SIG_SPECIFIC_SICODES_MASK},
    libs::mutex::Mutex,
    process::pid::PidType,
};
use alloc::sync::Arc;
use system_error::SystemError;

use super::file::{File, FileFlags};

pub const FASYNC_POLL_IN: i64 = 0x00000001 | 0x00000040;
pub const FASYNC_POLL_OUT: i64 = 0x00000004 | 0x00000100 | 0x00000200;
pub const FASYNC_POLL_MSG: i64 = 0x00000001 | 0x00000040 | 0x00000400;
pub const FASYNC_POLL_ERR: i64 = 0x00000008;
pub const FASYNC_POLL_PRI: i64 = 0x00000002 | 0x00000080;
pub const FASYNC_POLL_HUP: i64 = 0x00000010 | 0x00000008;

fn poll_band_to_sig_code(band: i64) -> Option<SigCode> {
    match band {
        FASYNC_POLL_IN => Some(SigCode::PollIn),
        FASYNC_POLL_OUT => Some(SigCode::PollOut),
        FASYNC_POLL_MSG => Some(SigCode::PollMsg),
        FASYNC_POLL_ERR => Some(SigCode::PollErr),
        FASYNC_POLL_PRI => Some(SigCode::PollPri),
        FASYNC_POLL_HUP => Some(SigCode::PollHup),
        _ => None,
    }
}

fn signal_has_specific_si_codes(sig: Signal) -> bool {
    sig != Signal::SIGIO_OR_POLL && SIG_SPECIFIC_SICODES_MASK.contains(Signal::into_sigset(sig))
}

pub fn fasync_band_from_epoll(events: EPollEventType) -> Option<i64> {
    if events.contains(EPollEventType::EPOLLHUP) {
        Some(FASYNC_POLL_HUP)
    } else if events.contains(EPollEventType::EPOLLERR) {
        Some(FASYNC_POLL_ERR)
    } else if events.contains(EPollEventType::EPOLLPRI) {
        Some(FASYNC_POLL_PRI)
    } else if events.contains(EPollEventType::EPOLLIN) {
        Some(FASYNC_POLL_IN)
    } else if events.contains(EPollEventType::EPOLLOUT) {
        Some(FASYNC_POLL_OUT)
    } else {
        None
    }
}

struct FAsyncSignalTarget {
    pcb: Arc<crate::process::ProcessControlBlock>,
    signum: i32,
    fd: i32,
    band: i64,
}

/// FAsyncItem represents a file that wants to receive SIGIO signals
/// when IO events occur on the underlying inode.
#[derive(Clone, Debug)]
pub struct FAsyncItem {
    /// Weak reference to the file
    file: Weak<File>,
    fd: i32,
}

impl FAsyncItem {
    pub fn new(file: Weak<File>, fd: i32) -> Self {
        Self { file, fd }
    }

    /// Get the file reference
    pub fn file(&self) -> Option<Arc<File>> {
        self.file.upgrade()
    }

    /// Check if the file is still alive
    #[allow(dead_code)]
    pub fn is_alive(&self) -> bool {
        self.file.strong_count() > 0
    }

    /// Get the weak reference to the file
    pub fn file_weak(&self) -> &Weak<File> {
        &self.file
    }

    pub fn fd(&self) -> i32 {
        self.fd
    }

    pub fn set_fd(&mut self, fd: i32) {
        self.fd = fd;
    }
}

/// List of FAsyncItems for an inode
pub type LockedFAsyncItemList = Mutex<Vec<FAsyncItem>>;

/// FAsyncItems manages the list of files that want SIGIO notifications
#[derive(Debug)]
pub struct FAsyncItems {
    items: LockedFAsyncItemList,
}

impl Default for FAsyncItems {
    fn default() -> Self {
        Self::new()
    }
}

impl FAsyncItems {
    /// Create a new FAsyncItems
    pub fn new() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
        }
    }

    /// Add a FAsyncItem
    pub fn add(&self, item: FAsyncItem) {
        let mut guard = self.items.lock();
        for old_item in guard.iter_mut() {
            if Weak::ptr_eq(old_item.file_weak(), item.file_weak()) {
                old_item.set_fd(item.fd());
                return;
            }
        }
        guard.push(item);
    }

    /// Remove a FAsyncItem by file reference
    pub fn remove(&self, file: &Weak<File>) {
        let mut guard = self.items.lock();
        guard.retain(|item| !Weak::ptr_eq(item.file_weak(), file));
    }

    /// Clear all items
    #[allow(dead_code)]
    pub fn clear(&self) {
        self.items.lock().clear();
    }

    /// Send SIGIO to all registered file owners
    /// This should be called when IO events occur (e.g., data becomes readable)
    pub fn send_sigio(&self, band: i64) {
        let mut targets = Vec::new();
        let guard = self.items.lock();
        for item in guard.iter() {
            if let Some(file) = item.file() {
                // Check if FASYNC is set
                if !file.flags().fasync() {
                    continue;
                }

                let owner = file.owner_snapshot();
                if let Some(pcb) = owner.pcb {
                    targets.push(FAsyncSignalTarget {
                        pcb,
                        signum: owner.signum,
                        fd: item.fd(),
                        band,
                    });
                }
            }
        }
        drop(guard);

        for target in targets {
            Self::send_sigio_to_process(target.pcb, target.signum, target.fd, target.band);
        }
    }

    /// Send SIGIO signal to a process
    fn send_sigio_to_process(
        pcb: Arc<crate::process::ProcessControlBlock>,
        signum: i32,
        fd: i32,
        band: i64,
    ) {
        let sig = if signum == 0 {
            Signal::SIGIO_OR_POLL
        } else {
            Signal::from(signum)
        };

        if sig == Signal::INVALID {
            return;
        }

        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        if signum == 0 {
            let _ = sig.send_signal_info_to_pcb(None, pcb, PidType::TGID);
        } else {
            let sig_code = if signal_has_specific_si_codes(sig) {
                SigCode::SigIO
            } else {
                poll_band_to_sig_code(band).unwrap_or(SigCode::SigIO)
            };
            let mut info = SigInfo::new(sig, 0, sig_code, SigType::SigPoll { fd, band });
            let _ = sig.send_signal_info_to_pcb(Some(&mut info), pcb, PidType::TGID);
        }

        compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }
}

pub fn set_file_fasync(file: &Arc<File>, fd: i32, enabled: bool) -> Result<(), SystemError> {
    let mut flags = file.flags();
    if enabled {
        flags.insert(FileFlags::FASYNC);
    } else {
        flags.remove(FileFlags::FASYNC);
    }

    file.set_flags(flags)?;

    if let Ok(pollable) = file.inode().as_pollable_inode() {
        let private_data = file.private_data.lock();
        if enabled {
            let item = FAsyncItem::new(Arc::downgrade(file), fd);
            let _ = pollable.add_fasync(item, &private_data);
        } else {
            let _ = pollable.remove_fasync(&Arc::downgrade(file), &private_data);
        }
    }

    Ok(())
}
