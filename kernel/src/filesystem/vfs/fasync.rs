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
    arch::ipc::signal::Signal, ipc::kill::send_signal_to_pcb, libs::spinlock::SpinLock,
    process::ProcessControlBlock,
};
use alloc::sync::Arc;

use super::file::File;

/// FAsyncItem represents a file that wants to receive SIGIO signals
/// when IO events occur on the underlying inode.
#[derive(Debug)]
pub struct FAsyncItem {
    /// Weak reference to the file
    file: Weak<File>,
}

impl FAsyncItem {
    pub fn new(file: Weak<File>) -> Self {
        Self { file }
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
}

/// List of FAsyncItems for an inode
pub type LockedFAsyncItemList = SpinLock<Vec<Arc<FAsyncItem>>>;

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
            items: SpinLock::new(Vec::new()),
        }
    }

    /// Add a FAsyncItem
    pub fn add(&self, item: Arc<FAsyncItem>) {
        self.items.lock_irqsave().push(item);
    }

    /// Remove a FAsyncItem by file reference
    pub fn remove(&self, file: &Weak<File>) {
        let mut guard = self.items.lock_irqsave();
        guard.retain(|item| !Weak::ptr_eq(item.file_weak(), file));
    }

    /// Clear all items
    #[allow(dead_code)]
    pub fn clear(&self) {
        self.items.lock_irqsave().clear();
    }

    /// Send SIGIO to all registered file owners
    /// This should be called when IO events occur (e.g., data becomes readable)
    pub fn send_sigio(&self) {
        let guard = self.items.lock_irqsave();
        for item in guard.iter() {
            if let Some(file) = item.file() {
                // Check if FASYNC is set
                if !file.flags().fasync() {
                    continue;
                }

                // Get the owner process
                let owner = file.get_owner();
                if let Some(pcb) = owner {
                    // Send SIGIO to the owner
                    Self::send_sigio_to_process(pcb);
                }
            }
        }
    }

    /// Send SIGIO signal to a process
    fn send_sigio_to_process(pcb: Arc<ProcessControlBlock>) {
        let sig = Signal::SIGIO_OR_POLL;

        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        let _ = send_signal_to_pcb(pcb, sig);

        compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }
}
