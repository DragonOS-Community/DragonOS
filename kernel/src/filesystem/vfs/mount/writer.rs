//! Mount writer admission, separate from topology and backend I/O locks.
//!
//! A superblock's short gate serializes both counters, mount holds and the
//! readonly transition. No guard returned here retains the gate. In particular
//! a topology transaction may try a hold, but must never wait for writers.
use super::{MountExternalGuard, MountFS, MountFlags, SuperBlockState};
use alloc::sync::Arc;
use core::sync::atomic::Ordering;
use system_error::SystemError;

/// Linux special_file(): these types write to a device/pipe/socket rather
/// than changing the inode's filesystem contents. DragonOS's additional
/// device types both report S_IFCHR to userspace.
pub(crate) fn is_special_file_type(file_type: super::super::FileType) -> bool {
    use super::super::FileType;
    matches!(
        file_type,
        FileType::CharDevice
            | FileType::BlockDevice
            | FileType::KvmDevice
            | FileType::FramebufferDevice
            | FileType::Pipe
            | FileType::Socket
    )
}

#[derive(Debug, Default)]
pub(super) struct WriterAdmissionState {
    writers: usize,
    readonly_transition: bool,
}

#[derive(Debug)]
pub struct MountWriteGuard {
    mount: Arc<MountFS>,
    // Layered filesystems admit their backing mounts before acquiring FS locks.
    backing: alloc::vec::Vec<MountWriteGuard>,
    _pin: MountExternalGuard,
}

impl MountWriteGuard {
    pub(crate) fn covers(&self, mount: &Arc<MountFS>) -> bool {
        Arc::ptr_eq(&self.mount, mount) || self.backing.iter().any(|writer| writer.covers(mount))
    }

    /// A distinct OFD needs its own writer, while dup/fork share the File.
    pub(crate) fn derive(&self) -> Result<Self, SystemError> {
        self.mount.want_write_from(&self._pin)
    }
}

impl Drop for MountWriteGuard {
    fn drop(&mut self) {
        let mut state = self.mount.super_block_state.writer_gate.lock();
        let writers = self.mount.writer_count.load(Ordering::Relaxed);
        assert!(writers > 0 && state.writers > 0, "mount writer underflow");
        self.mount
            .writer_count
            .store(writers - 1, Ordering::Relaxed);
        state.writers -= 1;
    }
}

#[derive(Debug)]
pub struct MountWriterHoldGuard {
    mount: Arc<MountFS>,
    held: bool,
}

impl MountWriterHoldGuard {
    /// All fallible preparation must precede this publication.
    pub fn commit_mount_flags(self, flags: MountFlags) {
        self.commit_attributes(flags, None);
    }

    /// Install a prepared mapping and attributes before allowing new writers.
    /// The caller holds the lifecycle lock and has checked one-time mapping
    /// eligibility. No backend operation or allocation belongs in this step.
    pub(crate) fn commit_attributes(
        mut self,
        flags: MountFlags,
        idmap: Option<Arc<super::idmap::MountIdmap>>,
    ) {
        let sb = &self.mount.super_block_state;
        {
            let _gate = sb.writer_gate.lock();
            if let Some(idmap) = idmap {
                self.mount.install_idmap_under_writer_gate(idmap);
            }
            *self.mount.mount_flags.write() = flags;
            self.mount.writer_hold.store(false, Ordering::Relaxed);
            self.held = false;
        }
        sb.writer_wait.wake_all();
    }
}

impl Drop for MountWriterHoldGuard {
    fn drop(&mut self) {
        if self.held {
            {
                let _gate = self.mount.super_block_state.writer_gate.lock();
                self.mount.writer_hold.store(false, Ordering::Relaxed);
            }
            self.mount.super_block_state.writer_wait.wake_all();
        }
    }
}

impl MountFS {
    /// Call before acquiring any topology, dentry or backend mutation lock.
    pub fn want_write(&self) -> Result<MountWriteGuard, SystemError> {
        self.want_write_with_pin(self.pin_operation_owner()?)
    }

    pub(crate) fn want_write_from(
        &self,
        pin: &MountExternalGuard,
    ) -> Result<MountWriteGuard, SystemError> {
        assert!(
            Arc::ptr_eq(&pin.mount(), &self.self_ref()),
            "writer pin belongs to another mount"
        );
        self.want_write_with_pin(pin.derive()?)
    }

    fn want_write_with_pin(&self, pin: MountExternalGuard) -> Result<MountWriteGuard, SystemError> {
        let mount = self.self_ref.upgrade().ok_or(SystemError::ESTALE)?;
        let mut pin = Some(pin);
        let mut admit = || {
            let mut state = self.super_block_state.writer_gate.lock();
            if state.readonly_transition || self.is_readonly() {
                return Some(Err(SystemError::EROFS));
            }
            if self.writer_hold.load(Ordering::Relaxed) {
                return None;
            }
            let count = self.writer_count.load(Ordering::Relaxed);
            self.writer_count.store(
                count.checked_add(1).expect("mount writer overflow"),
                Ordering::Relaxed,
            );
            state.writers = state.writers.checked_add(1).expect("SB writer overflow");
            Some(Ok(MountWriteGuard {
                mount: mount.clone(),
                backing: alloc::vec::Vec::new(),
                _pin: pin.take().unwrap(),
            }))
        };
        // The overwhelmingly common unheld path needs no waiter allocation.
        let mut writer = match admit() {
            Some(result) => result,
            None => self.super_block_state.writer_wait.wait_until(admit),
        }?;
        writer.backing = self.inner_filesystem.prepare_write()?;
        Ok(writer)
    }

    pub fn try_hold_writers(&self) -> Result<MountWriterHoldGuard, SystemError> {
        let mount = self.self_ref.upgrade().ok_or(SystemError::ESTALE)?;
        let _gate = self.super_block_state.writer_gate.lock();
        if self.writer_count.load(Ordering::Relaxed) != 0
            || self.writer_hold.load(Ordering::Relaxed)
        {
            return Err(SystemError::EBUSY);
        }
        // Never expose a hold when an existing writer could need to nest.
        self.writer_hold.store(true, Ordering::Relaxed);
        Ok(MountWriterHoldGuard { mount, held: true })
    }

    pub fn has_mount_writers(&self) -> bool {
        let _gate = self.super_block_state.writer_gate.lock();
        self.writer_count.load(Ordering::Relaxed) != 0
    }
}

#[derive(Debug)]
pub struct SuperBlockReadonlyTransition {
    state: Arc<SuperBlockState>,
    active: bool,
}

impl SuperBlockReadonlyTransition {
    pub fn commit(mut self, flags: MountFlags) {
        {
            let mut gate = self.state.writer_gate.lock();
            self.state.set_flags_under_writer_gate(flags);
            gate.readonly_transition = false;
            self.active = false;
        }
        self.state.writer_wait.wake_all();
    }
}

impl Drop for SuperBlockReadonlyTransition {
    fn drop(&mut self) {
        if self.active {
            self.state.writer_gate.lock().readonly_transition = false;
            self.state.writer_wait.wake_all();
        }
    }
}

impl SuperBlockState {
    pub fn begin_readonly_transition(
        self: &Arc<Self>,
        readonly: bool,
        force: bool,
    ) -> Result<Option<SuperBlockReadonlyTransition>, SystemError> {
        let mut gate = self.writer_gate.lock();
        if self.flags().contains(MountFlags::RDONLY) == readonly {
            return Ok(None);
        }
        if gate.readonly_transition
            || (readonly
                && !force
                && (gate.writers != 0 || self.pending_removals.load(Ordering::Acquire) != 0))
        {
            return Err(SystemError::EBUSY);
        }
        gate.readonly_transition = true;
        Ok(Some(SuperBlockReadonlyTransition {
            state: self.clone(),
            active: true,
        }))
    }

    pub fn has_writers(&self) -> bool {
        self.writer_gate.lock().writers != 0
    }

    /// Attached to a canonical zero-link inode's existing reclaim capability.
    pub(crate) fn pending_removal(self: &Arc<Self>) -> PendingRemovalGuard {
        self.pending_removals.fetch_add(1, Ordering::AcqRel);
        PendingRemovalGuard {
            state: self.clone(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct PendingRemovalGuard {
    state: Arc<SuperBlockState>,
}

impl Drop for PendingRemovalGuard {
    fn drop(&mut self) {
        let count = self.state.pending_removals.fetch_sub(1, Ordering::AcqRel);
        assert!(count > 0, "pending removal underflow");
    }
}
