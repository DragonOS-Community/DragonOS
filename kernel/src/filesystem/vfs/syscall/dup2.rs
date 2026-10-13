use system_error::SystemError;

use crate::filesystem::vfs::{
    fdtable::{DroppedFd, FileDescriptorTable},
    file::FileFlags,
};

pub fn do_dup3(
    oldfd: i32,
    newfd: i32,
    flags: FileFlags,
    fd_table: &FileDescriptorTable,
    soft_limit: usize,
) -> Result<(usize, Option<DroppedFd>), SystemError> {
    if newfd < 0 || newfd as usize >= soft_limit {
        return Err(SystemError::EBADF);
    }

    let cloexec = flags.contains(FileFlags::O_CLOEXEC);
    // dup2/dup3 read the source at the final table transaction after expansion,
    // unlike ordinary dup's earlier fget snapshot.
    let current = crate::process::ProcessManager::current_pcb();
    let (res, dropped) = crate::filesystem::vfs::file::File::recover_io(|| {
        fd_table.duplicate_exact(
            oldfd,
            newfd,
            cloexec,
            soft_limit,
            &crate::cgroup::memory::object_for_task(&current),
        )
    })?;
    return Ok((res as usize, dropped));
}
