//! Lifetime-safe file-descriptor handle for socket system calls.
//!
//! Linux shelters a blocking socket system call from a concurrent `close(2)` by
//! holding a reference to the open file description for the whole call
//! (`sockfd_lookup_light()` in `net/socket.c` pairs `fdget()` with `fdput()`).
//! As long as that reference exists, `close()` only detaches the descriptor from
//! the table and drops one reference; `__fput()` runs after the syscall returns.
//!
//! DragonOS models the same thing with `Arc<File>`: `File::drop()` runs
//! `IndexNode::close()`, which only calls `Socket::do_close()` when
//! `open_file_counter` reaches zero. A socket system call that looks up the fd
//! and then drops the `Arc<File>` therefore exposes the socket to being torn
//! down mid-call — the source of the blocked-`accept()` panic.
//!
//! The receive path already had to solve this for its own waits (see
//! `do_recvmsg_with_sock()` in `sys_recvmsg.rs`, which `recvmmsg` reuses for a
//! whole batch); [`SocketFdRef`] generalises the same rule to every socket entry
//! point.

use alloc::sync::Arc;
use system_error::SystemError;

use crate::filesystem::vfs::file::{File, FileFlags};
use crate::filesystem::vfs::IndexNode;
use crate::net::socket::Socket;
use crate::process::ProcessManager;

/// An open file description obtained from a socket file descriptor.
///
/// A value of this type keeps the socket alive for as long as it is held, so a
/// blocking operation may derive `&dyn Socket` from it and sleep without risking
/// that another thread's `close(2)` destroys the socket underneath.
///
/// Bind the handle to a local variable and borrow the socket from it, so the
/// borrow checker rejects any attempt to use a temporary that is dropped before
/// the blocking operation completes:
///
/// ```ignore
/// let sock = SocketFdRef::from_fd(fd)?;
/// let sk = sock.socket()?;
/// let n = sk.sendmsg(...)?; // `sock` keeps the socket alive while blocked.
/// ```
pub struct SocketFdRef {
    /// The open file description that must outlive the call. Dropping it may
    /// trigger `do_close()`, so it must not be released while the operation is
    /// still in flight.
    file: Arc<File>,
    /// The socket inode behind the same open file description. Held separately
    /// so that [`SocketFdRef::socket`] can hand out a borrow tied to this value.
    inode: Arc<dyn IndexNode>,
}

impl SocketFdRef {
    /// Look up `fd` in the current process and validate that it is a socket.
    ///
    /// All validation happens here so that callers can treat the handle as an
    /// already-checked socket: `EBADF` for unknown descriptors and `ENOTSOCK`
    /// for non-socket files, matching the error precedence of Linux
    /// `sockfd_lookup_light()`.
    ///
    /// Only `IndexNode::as_socket()` decides "is this a socket": it is the same
    /// single type test Linux performs in `file_to_socket()` (which compares
    /// `file->f_op`), and every `T: Socket` inode answers it through the blanket
    /// impl in `net/socket/inode.rs`, so a separate `FileType` comparison would
    /// be redundant state to keep in sync.
    ///
    /// The descriptor table lock is released before this function returns: the
    /// `Arc<File>` is cloned while the table read lock is held (cloning cannot
    /// run `close()`), and the fd table defers all close work to
    /// `DroppedFd::finish_close()`, which its callers run after dropping the
    /// lock.
    pub fn from_fd(fd: i32) -> Result<Self, SystemError> {
        let file = {
            let binding = ProcessManager::current_pcb().fd_table();
            let guard = binding.read();
            guard.get_file_by_fd(fd).ok_or(SystemError::EBADF)?
        };

        let inode = file.inode();
        if inode.as_socket().is_none() {
            return Err(SystemError::ENOTSOCK);
        }

        Ok(Self { file, inode })
    }

    /// Borrow the socket.
    ///
    /// `SocketFdRef` never panics: the inode was checked in [`Self::from_fd`],
    /// but a `Result` keeps the type honest for callers and lets borrow checking
    /// tie the returned reference to `self`.
    #[inline]
    pub fn socket(&self) -> Result<&dyn Socket, SystemError> {
        self.inode.as_socket().ok_or(SystemError::ENOTSOCK)
    }

    /// The open file description's `O_NONBLOCK` status flag.
    ///
    /// This is the `fcntl(F_SETFL)` state of the descriptor (`struct file` in
    /// Linux), which is distinct from per-`send`/`recv` `MSG_DONTWAIT` flags.
    #[inline]
    pub fn is_nonblocking(&self) -> bool {
        self.file.flags().contains(FileFlags::O_NONBLOCK)
    }
}
