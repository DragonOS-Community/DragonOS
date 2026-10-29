//! Persistent FIFO identity, separate from a first-open/last-close pipe runtime.

use super::*;
use alloc::{string::String, sync::Arc};
use core::{
    any::Any,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use system_error::SystemError;

#[derive(Debug)]
struct Active {
    runtime: Arc<PipeRuntime>,
    files: usize,
}

#[derive(Debug)]
pub struct LockedPipeInode {
    metadata: SpinLock<Metadata>,
    metadata_transaction: Mutex<()>,
    active: Mutex<Option<Active>>,
    fifo: AtomicBool,
    watches: AtomicUsize,
}

impl LockedPipeInode {
    fn shell(uid: usize, gid: usize, fifo: bool) -> Self {
        let time = PosixTimeSpec::now();
        Self {
            metadata: SpinLock::new(Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: time,
                mtime: time,
                ctime: time,
                btime: PosixTimeSpec::default(),
                file_type: FileType::Pipe,
                mode: InodeMode::S_IFIFO | InodeMode::from_bits_truncate(0o600),
                nlinks: 1,
                uid,
                gid,
                raw_dev: Default::default(),
                flags: InodeFlags::empty(),
            }),
            active: Mutex::new(None),
            metadata_transaction: Mutex::new(()),
            fifo: AtomicBool::new(fifo),
            watches: AtomicUsize::new(0),
        }
    }

    /// Metadata projection only; native inode construction must not allocate
    /// the running FIFO's ACCOUNT storage under a filesystem lookup guard.
    pub fn new_fifo_shell() -> Arc<Self> {
        Arc::new(Self::shell(0, 0, true))
    }

    pub fn try_new_anonymous(
        uid: usize,
        gid: usize,
        creator: &Arc<ObjectAccount>,
    ) -> Result<Arc<Self>, MemoryAllocationError> {
        let shell = Arc::try_new(Self::shell(uid, gid, false)).map_err(|_| SystemError::ENOMEM)?;
        let runtime = PipeRuntime::try_new(false, creator)?;
        *shell.active.lock() = Some(Active { runtime, files: 0 });
        Ok(shell)
    }

    pub fn set_fifo(&self) {
        self.fifo.store(true, Ordering::Relaxed);
    }

    pub(crate) fn runtime(data: &FilePrivateData) -> Result<Arc<PipeRuntime>, SystemError> {
        match data {
            FilePrivateData::Pipefs(data) => data.runtime.clone().ok_or(SystemError::EBADF),
            _ => Err(SystemError::EBADF),
        }
    }

    fn acquire_file(&self) -> Result<Arc<PipeRuntime>, MemoryAllocationError> {
        {
            let mut active = self.active.lock();
            if let Some(active) = active.as_mut() {
                active.files = active.files.checked_add(1).ok_or(SystemError::EOVERFLOW)?;
                return Ok(active.runtime.clone());
            }
        }
        let creator =
            crate::cgroup::memory::object_for_task(&crate::process::ProcessManager::current_pcb());
        let candidate = PipeRuntime::try_new(self.fifo.load(Ordering::Relaxed), &creator);
        let mut guard = self.active.lock();
        // A successful competing first open also wins over a failed proposal.
        if let Some(active) = guard.as_mut() {
            active.files = active.files.checked_add(1).ok_or(SystemError::EOVERFLOW)?;
            let runtime = active.runtime.clone();
            drop(guard);
            // End the slot guard before canceling potentially charged storage.
            drop(candidate);
            return Ok(runtime);
        }
        let candidate = match candidate {
            Ok(candidate) => candidate,
            Err(error) => {
                drop(guard);
                return Err(error);
            }
        };
        *guard = Some(Active {
            runtime: candidate.clone(),
            files: 1,
        });
        drop(guard);
        Ok(candidate)
    }

    fn release_file(&self, runtime: &Arc<PipeRuntime>) {
        let retired = {
            let mut active = self.active.lock();
            let current = active.as_mut().expect("open pipe runtime remains active");
            assert!(Arc::ptr_eq(&current.runtime, runtime));
            current.files = current
                .files
                .checked_sub(1)
                .expect("pipe file pin underflow");
            if current.files == 0 {
                active.take()
            } else {
                None
            }
        };
        drop(retired);
    }
}

impl IndexNode for LockedPipeInode {
    fn fsnotify_watch_count(&self) -> Option<&AtomicUsize> {
        Some(&self.watches)
    }
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, MemoryAllocationError> {
        Self::runtime(&data)?.read_at(offset, len, buf, data)
    }
    fn user_read_mode(&self) -> crate::filesystem::vfs::UserReadMode {
        crate::filesystem::vfs::UserReadMode::Iterator
    }
    fn read_user_at(
        &self,
        _offset: usize,
        len: usize,
        writer: &mut crate::syscall::user_buffer::UserBuffer<'_>,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<Option<usize>, SystemError> {
        Self::runtime(&data)?.read_user(len, writer, data).map(Some)
    }
    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, MemoryAllocationError> {
        Self::runtime(&data)?.write_at(offset, len, buf, data)
    }
    fn write_user_buffer_at(
        &self,
        _offset: usize,
        len: usize,
        source: &crate::syscall::user_buffer::UserBuffer<'_>,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<Option<usize>, MemoryAllocationError> {
        Self::runtime(&data)?
            .write_user_buffer(len, source, data)
            .map(Some)
    }
    fn open(
        &self,
        mut data: MutexGuard<FilePrivateData>,
        flags: &FileFlags,
    ) -> Result<(), MemoryAllocationError> {
        if flags.contains(FileFlags::O_PATH) {
            *data = FilePrivateData::Pipefs(PipeFsPrivateData::new(*flags));
            return Ok(());
        }
        let runtime = self.acquire_file()?;
        *data = FilePrivateData::Pipefs(PipeFsPrivateData {
            flags: *flags,
            runtime: Some(runtime.clone()),
        });
        let result = runtime.open(data, flags);
        if result.is_err() {
            self.release_file(&runtime);
        }
        result
    }
    fn close(&self, data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        if let FilePrivateData::Pipefs(value) = &*data {
            if value.flags.contains(FileFlags::O_PATH) {
                return Ok(());
            }
        }
        let runtime = Self::runtime(&data)?;
        let result = runtime.close(data);
        self.release_file(&runtime);
        result
    }
    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.metadata.lock().clone())
    }
    fn set_metadata_masked(
        &self,
        metadata: &Metadata,
        mask: SetMetadataMask,
    ) -> Result<(), MemoryAllocationError> {
        let _transaction = self.metadata_transaction.lock();
        merge_metadata_masked(&mut self.metadata.lock(), metadata, mask);
        Ok(())
    }
    fn update_metadata_masked(
        &self,
        update: &mut MetadataUpdate<'_>,
    ) -> Result<SetMetadataMask, MemoryAllocationError> {
        let _transaction = self.metadata_transaction.lock();
        let current = self.metadata.lock().clone();
        let (next, mask) = update(&current)?;
        merge_metadata_masked(&mut self.metadata.lock(), &next, mask);
        Ok(mask)
    }
    fn ioctl(
        &self,
        cmd: u32,
        value: usize,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, MemoryAllocationError> {
        Self::runtime(&data)?.ioctl(cmd, value, data)
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        PipeFS::instance()
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Ok(self)
    }
    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(String::from("pipe"))
    }
}

impl PollableInode for LockedPipeInode {
    fn poll(&self, data: &FilePrivateData) -> Result<usize, SystemError> {
        Self::runtime(data)?.poll(data)
    }
    fn add_epitem(&self, item: Arc<EPollItem>, data: &FilePrivateData) -> Result<(), SystemError> {
        Self::runtime(data)?.add_epitem(item, data)
    }
    fn remove_epitem(
        &self,
        item: &Arc<EPollItem>,
        data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        Self::runtime(data)?.remove_epitem(item, data)
    }
    fn add_fasync(&self, item: FAsyncItem, data: &FilePrivateData) -> Result<(), SystemError> {
        Self::runtime(data)?.add_fasync(item, data)
    }
    fn remove_fasync(
        &self,
        file: &Weak<crate::filesystem::vfs::file::File>,
        data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        Self::runtime(data)?.remove_fasync(file, data)
    }
    fn release_fasync(
        &self,
        file: &crate::filesystem::vfs::file::File,
        data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        Self::runtime(data)?.release_fasync(file, data)
    }
}
