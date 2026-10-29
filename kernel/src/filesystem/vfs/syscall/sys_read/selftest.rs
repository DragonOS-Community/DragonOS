//! Exercise count admission through the real read(2) userspace branch without
//! allocating or copying MAX_RW_COUNT bytes. The inode observes dispatch only.
use super::{read_into_user_buffer, MAX_RW_COUNT};
use alloc::sync::Arc;
use core::{
    any::Any,
    sync::atomic::{AtomicUsize, Ordering},
};
use system_error::SystemError;

use crate::{
    arch::MMArch,
    filesystem::{
        anon_inode::{anon_inode_metadata, AnonInodeFs},
        vfs::{
            fdtable::FileDescriptorTable,
            file::{File, FileFlags, FilePrivateData, PreparedFile},
            FileSystem, IndexNode, InodeMode, Metadata, UserReadMode,
        },
    },
    libs::mutex::MutexGuard,
    mm::{allocation::MemoryAllocationError, MemoryManagementArch},
    process::ProcessManager,
    syscall::user_buffer::UserBuffer,
};

#[derive(Debug, Default)]
struct CountedReader {
    calls: AtomicUsize,
    requested: AtomicUsize,
}

impl IndexNode for CountedReader {
    fn read_at(
        &self,
        _: usize,
        _: usize,
        _: &mut [u8],
        _: MutexGuard<FilePrivateData>,
    ) -> Result<usize, MemoryAllocationError> {
        Err(SystemError::EIO.into())
    }

    fn write_at(
        &self,
        _: usize,
        _: usize,
        _: &[u8],
        _: MutexGuard<FilePrivateData>,
    ) -> Result<usize, MemoryAllocationError> {
        Err(SystemError::EIO.into())
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(anon_inode_metadata(InodeMode::from_bits_truncate(0o600)))
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        AnonInodeFs::instance()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn close(&self, _: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn user_read_mode(&self) -> UserReadMode {
        UserReadMode::Scalar
    }

    fn read_user_at(
        &self,
        _: usize,
        len: usize,
        writer: &mut UserBuffer<'_>,
        _: MutexGuard<FilePrivateData>,
    ) -> Result<Option<usize>, SystemError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.requested.store(len, Ordering::Relaxed);
        if writer.len() != len {
            return Err(SystemError::EIO);
        }
        Ok(Some(0))
    }
}

struct InstalledReader {
    table: Arc<FileDescriptorTable>,
    fd: i32,
}

impl InstalledReader {
    fn new(inode: Arc<CountedReader>, flags: FileFlags) -> Result<Self, SystemError> {
        let storage = PreparedFile::new_recovering()?;
        let file = Arc::try_new(File::new_prepared_recovering(inode, flags, storage)?)
            .map_err(|_| SystemError::ENOMEM)?;
        let task = ProcessManager::current_pcb();
        let table = task.fd_table();
        let owner = crate::cgroup::memory::object_for_task(&task);
        let fd = File::recover_io(|| {
            table.alloc_fd_arc(file.clone(), true, task.nofile_soft_limit(), &owner)
        })?;
        Ok(Self { table, fd })
    }
}

impl Drop for InstalledReader {
    fn drop(&mut self) {
        if let Ok(dropped) = self.table.drop_fd(self.fd) {
            let _ = dropped.finish_close();
        }
    }
}

fn check() -> Result<bool, SystemError> {
    let inode = Arc::try_new(CountedReader::default()).map_err(|_| SystemError::ENOMEM)?;
    let reader = InstalledReader::new(inode.clone(), FileFlags::O_RDONLY)?;
    let pointer = MMArch::PAGE_SIZE as *mut u8;
    for (index, count) in [
        MAX_RW_COUNT - 1,
        MAX_RW_COUNT,
        MAX_RW_COUNT + MMArch::PAGE_SIZE,
    ]
    .into_iter()
    .enumerate()
    {
        if read_into_user_buffer(reader.fd, pointer, count) != Ok(0)
            || inode.requested.load(Ordering::Relaxed) != count.min(MAX_RW_COUNT)
            || inode.calls.load(Ordering::Relaxed) != index + 1
        {
            return Ok(false);
        }
    }

    // The original range crosses USER_END, while its capped range stays below
    // it. Rejection must precede both clipping and the inode callback.
    let crossing = MMArch::USER_END_VADDR.data() - MAX_RW_COUNT - MMArch::PAGE_SIZE / 2;
    if read_into_user_buffer(
        reader.fd,
        crossing as *mut u8,
        MAX_RW_COUNT + MMArch::PAGE_SIZE,
    ) != Err(SystemError::EFAULT)
        || read_into_user_buffer(reader.fd, pointer, usize::MAX) != Err(SystemError::EFAULT)
        || inode.calls.load(Ordering::Relaxed) != 3
    {
        return Ok(false);
    }

    let writer = InstalledReader::new(inode.clone(), FileFlags::O_WRONLY)?;
    Ok(
        read_into_user_buffer(writer.fd, pointer, usize::MAX) == Err(SystemError::EBADF)
            && inode.calls.load(Ordering::Relaxed) == 3,
    )
}

pub(super) fn run() -> bool {
    check().unwrap_or(false)
}
