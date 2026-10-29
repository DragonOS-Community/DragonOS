//! Shared superblock active references and the single final shutdown path.
//! An fs_context owns a root before it owns a mount entry (Linux s_active).

use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

use super::{FileSystem, MountFlags, SuperBlockState, UserNamespace};
use crate::exception::workqueue::{schedule_work, Work};

/// A root held by a creation context participates in SB lifetime without
/// making an existing mount busy. Mounts and contexts use the same pin count.
#[derive(Debug)]
pub(crate) struct SuperBlockActiveGuard {
    fs: Arc<dyn FileSystem>,
    state: Arc<SuperBlockState>,
}

impl SuperBlockActiveGuard {
    pub(crate) fn acquire_shared(
        fs: &Arc<dyn FileSystem>,
        flags: MountFlags,
        owner: &Arc<UserNamespace>,
    ) -> Result<Option<Self>, SystemError> {
        let Some(state) = fs.shared_mount_superblock_state(flags) else {
            return Ok(None);
        };
        if !Arc::ptr_eq(state.owner_user_ns(), owner) {
            return Err(SystemError::EBUSY);
        }
        if !state.try_add_external_pin() {
            return Err(SystemError::ESTALE);
        }
        let guard = Self {
            fs: fs.clone(),
            state,
        };
        if let Some(domain) = fs.page_cache_writeback_domain() {
            domain.bind(fs, &guard.state)?;
        }
        Ok(Some(guard))
    }
}

impl Drop for SuperBlockActiveGuard {
    fn drop(&mut self) {
        if self.state.remove_external_pin() {
            let fs = self.fs.clone();
            let state = self.state.clone();
            schedule_work(Work::new(move || finish_shutdown(&fs, &state)));
        }
    }
}

pub(super) fn finish_shutdown(fs: &Arc<dyn FileSystem>, sb_state: &Arc<SuperBlockState>) {
    let _umount_guard = sb_state.umount_write();
    fs.close_external_inode_admission();
    if let Err(err) = sync_filesystem_locked(fs, sb_state) {
        log::warn!("final superblock sync failed during umount: {:?}", err);
        sb_state.record_wb_error(err);
    }
    if let Err(err) = fs.quiesce_page_cache_producers() {
        log::warn!("final page-cache producer quiesce failed: {:?}", err);
        sb_state.record_wb_error(err);
    }
    if let Some(domain) = fs.page_cache_writeback_domain() {
        domain.close_background_admission();
        if let Err(err) = domain.wait_drained() {
            log::warn!("final page-cache I/O drain failed: {:?}", err);
            sb_state.record_wb_error(err);
        }
        for cache in domain.snapshot() {
            if let Err(err) = cache.retire_for_domain() {
                log::warn!("final page-cache retirement failed: {:?}", err);
                sb_state.record_wb_error(err);
            }
        }
    }
    if let Err(err) = fs.shrink_inode_cache_for_shutdown() {
        log::warn!("final inode cache shrink failed: {:?}", err);
        sb_state.record_wb_error(err);
    }
    if let Err(err) = fs.quiesce_async_inode_work() {
        log::warn!("final asynchronous inode quiesce failed: {:?}", err);
        sb_state.record_wb_error(err);
    }
    let epoch = fs.seal_eviction_queue();
    if let Err(err) = fs.drain_evictions_through(epoch) {
        log::warn!("final superblock eviction drain failed: {:?}", err);
        sb_state.record_wb_error(err);
    }
    sb_state.drain_fsnotify_objects_for_unmount();
    fs.on_umount();
    sb_state.finish_shutdown();
}

pub(super) fn sync_inodes(
    fs: &Arc<dyn FileSystem>,
    sb_state: &SuperBlockState,
) -> Result<(), SystemError> {
    let Some(domain) = fs.page_cache_writeback_domain() else {
        return Ok(());
    };
    let mut last_err = Ok(());
    let mut members = domain
        .snapshot()
        .into_iter()
        .filter(|cache| cache.has_dirty_or_writeback_work());
    let mut pending = Vec::new();
    pending
        .try_reserve_exact(crate::filesystem::page_cache::DOMAIN_WRITEBACK_WINDOW)
        .map_err(|_| SystemError::ENOMEM)?;
    loop {
        let mut count = 0;
        for page_cache in members
            .by_ref()
            .take(crate::filesystem::page_cache::DOMAIN_WRITEBACK_WINDOW)
        {
            count += 1;
            match page_cache.start_sync_for_domain() {
                Ok(writeback) => pending.push(writeback),
                Err(error) => {
                    sb_state.record_wb_error(error.clone());
                    last_err = Err(error);
                }
            }
        }
        if count == 0 {
            break;
        }
        // Submit across mappings before forcing durable completion. An
        // outer guard would seal each small file before the next starts.
        let _sync_request = fs.begin_sync_writeback();
        for writeback in pending.drain(..) {
            if let Err(error) = writeback.finish() {
                log::warn!("sync_inodes_of_mount: page cache sync failed: {:?}", error);
                sb_state.record_wb_error(error.clone());
                last_err = Err(error);
            }
        }
    }
    last_err
}

/// The caller holds umount_lock. MountFS's blockdev-sync hook is currently a
/// no-op; data and metadata submission remain owned by the filesystem domain.
pub(super) fn sync_filesystem_locked(
    fs: &Arc<dyn FileSystem>,
    state: &SuperBlockState,
) -> Result<(), SystemError> {
    if state.flags().contains(MountFlags::RDONLY) {
        return Ok(());
    }
    let mut last_err = sync_inodes(fs, state);
    if let Err(error) = fs.sync_fs(false) {
        state.record_wb_error(error.clone());
        return Err(error);
    }
    if let Err(error) = sync_inodes(fs, state) {
        last_err = Err(error);
    }
    if let Err(error) = fs.sync_fs(true) {
        state.record_wb_error(error.clone());
        return Err(error);
    }
    last_err
}
