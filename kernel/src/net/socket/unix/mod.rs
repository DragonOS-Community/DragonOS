pub mod datagram;
pub mod ns;
pub mod ring_buffer;
pub mod stream;
pub mod utils;

use system_error::SystemError;

use self::utils::*;

use super::PSOCK;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::{
    filesystem::vfs::{
        utils::{rsplit_path, DName},
        InodeMode, VFS_MAX_FOLLOW_SYMLINK_TIMES,
    },
    net::socket::{
        endpoint::Endpoint,
        unix::{datagram::UnixDatagramSocket, ns::AbstractHandle, stream::UnixStreamSocket},
        Socket,
    },
    process::ProcessManager,
};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::hash::Hash;

/// Unix domain credential payload for SCM_CREDENTIALS.
///
/// Matches Linux `struct ucred { pid_t pid; uid_t uid; gid_t gid; }` layout on 64-bit.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UCred {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

/// Return current task credentials used for unix-domain SCM_CREDENTIALS.
pub fn current_ucred() -> UCred {
    let pcb = ProcessManager::current_pcb();
    let cred = pcb.cred();
    UCred {
        pid: pcb.raw_tgid().data() as i32,
        uid: cred.uid.data() as u32,
        gid: cred.gid.data() as u32,
    }
}

/// Linux behavior used by gVisor tests when credentials were not attached at send time.
pub const fn nobody_ucred() -> UCred {
    UCred {
        pid: 0,
        uid: 65534,
        gid: 65534,
    }
}

#[derive(Debug, Clone)]
pub enum UnixEndpoint {
    File(String),
    /// Abstract namespace address payload (sun_path bytes after the leading NUL).
    ///
    /// Linux treats it as a length-delimited binary name (may contain embedded NULs).
    Abstract(Vec<u8>),
    Unnamed,
}

impl TryFrom<Endpoint> for UnixEndpoint {
    type Error = SystemError;

    fn try_from(value: Endpoint) -> Result<Self, Self::Error> {
        match value {
            Endpoint::Unix(ep) => Ok(ep),
            _ => Err(SystemError::EINVAL),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum UnixEndpointBound {
    Path(DName),
    Abstract(Arc<AbstractHandle>),
}

impl PartialEq for UnixEndpointBound {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (UnixEndpointBound::Path(path1), UnixEndpointBound::Path(path2)) => path1 == path2,
            (UnixEndpointBound::Abstract(handle1), UnixEndpointBound::Abstract(handle2)) => {
                handle1.nsid() == handle2.nsid() && handle1.name() == handle2.name()
            }
            _ => false,
        }
    }
}

impl Eq for UnixEndpointBound {}

impl Ord for UnixEndpointBound {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        match (self, other) {
            (UnixEndpointBound::Path(path1), UnixEndpointBound::Path(path2)) => path1.cmp(path2),
            (UnixEndpointBound::Abstract(handle1), UnixEndpointBound::Abstract(handle2)) => {
                (handle1.nsid(), handle1.name()).cmp(&(handle2.nsid(), handle2.name()))
            }
            (UnixEndpointBound::Path(_), UnixEndpointBound::Abstract(_)) => {
                core::cmp::Ordering::Less
            }
            (UnixEndpointBound::Abstract(_), UnixEndpointBound::Path(_)) => {
                core::cmp::Ordering::Greater
            }
        }
    }
}

impl PartialOrd for UnixEndpointBound {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Hash for UnixEndpointBound {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        match self {
            UnixEndpointBound::Path(path) => {
                path.hash(state);
            }
            UnixEndpointBound::Abstract(handle) => {
                handle.nsid().hash(state);
                handle.name().hash(state);
            }
        }
    }
}

impl From<UnixEndpointBound> for UnixEndpoint {
    fn from(endpoint: UnixEndpointBound) -> Self {
        match endpoint {
            UnixEndpointBound::Path(path) => UnixEndpoint::File(String::from(path.as_ref())),
            UnixEndpointBound::Abstract(handle) => UnixEndpoint::Abstract(handle.name().to_vec()),
        }
    }
}

impl From<Option<UnixEndpointBound>> for UnixEndpoint {
    fn from(endpoint: Option<UnixEndpointBound>) -> Self {
        match endpoint {
            Some(ep) => ep.into(),
            None => UnixEndpoint::Unnamed,
        }
    }
}

impl<T: Into<UnixEndpoint>> From<T> for Endpoint {
    fn from(endpoint: T) -> Self {
        Endpoint::Unix(endpoint.into())
    }
}

impl UnixEndpoint {
    pub(super) fn bind_in(
        self,
        netns: &Arc<NetNamespace>,
    ) -> Result<UnixEndpointBound, SystemError> {
        let bound = match self {
            Self::Unnamed => UnixEndpointBound::Abstract(
                netns
                    .unix_abstract_table()
                    .alloc_ephemeral_abstract_name()?,
            ),
            Self::File(path) => {
                let (filename, parent_path) = rsplit_path(&path);
                // 查找父目录
                let parent_inode = ProcessManager::current_mntns()
                    .root_inode()
                    .lookup_follow_symlink(
                        parent_path.unwrap_or("/"),
                        VFS_MAX_FOLLOW_SYMLINK_TIMES,
                    )?;
                // 创建 socket inode
                let inode = parent_inode
                    .create(
                        filename,
                        crate::filesystem::vfs::FileType::Socket,
                        InodeMode::S_IWUSR,
                    )
                    .map_err(|e| match e {
                        // Linux/Posix bind 语义：地址已被占用应返回 EADDRINUSE。
                        // VFS 创建节点遇到同名条目通常返回 EEXIST，需要在 socket 层进行语义映射。
                        SystemError::EEXIST => SystemError::EADDRINUSE,
                        other => other,
                    })?;
                UnixEndpointBound::Path(DName::from(inode.absolute_path()?))
            }
            Self::Abstract(name) => UnixEndpointBound::Abstract(
                netns
                    .unix_abstract_table()
                    .create_abstract_name_bytes(&name)?,
            ),
        };

        Ok(bound)
    }

    pub(super) fn bind_unnamed(&self) -> Result<(), SystemError> {
        if matches!(self, UnixEndpoint::Unnamed) {
            return Ok(());
        }
        Err(SystemError::EINVAL)
    }

    pub(super) fn connect_in(
        &self,
        netns: &Arc<NetNamespace>,
    ) -> Result<UnixEndpointBound, SystemError> {
        let bound = match self {
            Self::Unnamed => return Err(SystemError::EINVAL),
            Self::Abstract(name) => UnixEndpointBound::Abstract(
                netns
                    .unix_abstract_table()
                    .lookup_abstract_name_bytes(name)?,
            ),
            Self::File(path) => {
                let (inode_begin, path) = crate::filesystem::vfs::utils::user_path_at(
                    &ProcessManager::current_pcb(),
                    crate::filesystem::vfs::fcntl::AtFlags::AT_FDCWD.bits(),
                    path.trim(),
                )?;
                let inode =
                    inode_begin.lookup_follow_symlink(&path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
                let abso_path = inode.absolute_path()?;
                // let inode = ProcessManager::current_mntns()
                //     .root_inode()
                //     .lookup_follow_symlink(path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
                UnixEndpointBound::Path(DName::from(abso_path))
            }
        };

        Ok(bound)
    }
}

pub fn create_unix_socket(
    socket_type: PSOCK,
    is_nonblocking: bool,
) -> Result<Arc<dyn Socket>, SystemError> {
    let socket: Arc<dyn Socket> = match socket_type {
        PSOCK::Stream => UnixStreamSocket::new(is_nonblocking, false),
        PSOCK::SeqPacket => UnixStreamSocket::new(is_nonblocking, true),
        PSOCK::Packet => UnixStreamSocket::new(is_nonblocking, true),
        PSOCK::Datagram => UnixDatagramSocket::new(is_nonblocking),
        // Linux supports AF_UNIX + SOCK_RAW and maps it to SOCK_DGRAM.
        // See Linux 6.6 net/unix/af_unix.c:unix_create().
        PSOCK::Raw => UnixDatagramSocket::new(is_nonblocking),
        _ => {
            return Err(SystemError::ESOCKTNOSUPPORT);
        }
    };
    Ok(socket)
}
