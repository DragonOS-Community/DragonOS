// System call handlers for network-related syscalls
pub mod socket_fd;
pub mod sys_accept;
pub mod sys_accept4;
pub mod sys_bind;
pub mod sys_connect;
pub mod sys_getpeername;
pub mod sys_getsockname;
pub mod sys_getsockopt;
pub mod sys_listen;
pub mod sys_recvfrom;
pub mod sys_recvmmsg;
pub mod sys_recvmsg;
pub mod sys_sendmmsg;
pub mod sys_sendmsg;
pub mod sys_sendto;
pub mod sys_setsockopt;
pub mod sys_shutdown;
pub mod sys_socket;
pub mod sys_socketpair;

/// Descriptor-tree backing is prepared before irreversible accept/pair
/// publication. A quota refusal reaches recovery only after table guards and
/// unpublished candidates have been retired by reserve().
pub(super) fn reserve_socket_descriptors<const N: usize>(
    task: &alloc::sync::Arc<crate::process::ProcessControlBlock>,
    cloexec: bool,
) -> Result<crate::filesystem::vfs::fdtable::FdReservation<N>, system_error::SystemError> {
    let mut recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::new();
    loop {
        let creator = crate::cgroup::memory::object_for_task(task);
        match task
            .fd_table()
            .reserve::<N>(task.nofile_soft_limit(), 0, cloexec, &creator)
        {
            Ok(reservation) => return Ok(reservation),
            Err(error) => error.recover(&mut recovery)?,
        }
    }
}

/// Prepare the open-file body before consuming a connection or publishing a
/// socket pair. Descriptor reservations are owners, not held fd-table locks.
pub(super) fn prepare_socket_file(
) -> Result<crate::filesystem::vfs::file::PreparedFile, system_error::SystemError> {
    let mut recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::new();
    loop {
        match crate::filesystem::vfs::file::PreparedFile::try_new() {
            Ok(prepared) => return Ok(prepared),
            Err(error) => error.recover(&mut recovery)?,
        }
    }
}

/// Finish the same newly created/accepted socket, never replay accept. The
/// current SocketCreate open hook only sets mode and increments its counter;
/// it cannot charge or fail after that increment. Generic typed hooks remain
/// guarded here against a future unpublished, side-effect-free admission.
pub(super) fn finish_socket_file(
    inode: alloc::sync::Arc<dyn crate::filesystem::vfs::IndexNode>,
    flags: crate::filesystem::vfs::file::FileFlags,
    mut prepared: crate::filesystem::vfs::file::PreparedFile,
) -> Result<crate::filesystem::vfs::file::File, system_error::SystemError> {
    let mut recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::new();
    loop {
        let result =
            crate::filesystem::vfs::file::File::new_socket_prepared(inode.clone(), flags, prepared);
        let error = match result {
            Ok(file) => return Ok(file),
            Err(error) => error,
        };
        let retry = error
            .recover(&mut recovery)
            .and_then(|()| prepare_socket_file());
        match retry {
            Ok(storage) => prepared = storage,
            Err(error) => {
                // The protocol stack can own a callback Arc after accept.
                // Dropping just this Arc does not close that transport. No
                // successful open exists, so inode.close would underflow.
                if let Some(socket) = inode.as_socket() {
                    let _ = socket.do_close();
                }
                return Err(error);
            }
        }
    }
}
