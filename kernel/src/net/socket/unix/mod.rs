pub mod datagram;
pub mod ns;
pub mod ring_buffer;
pub mod stream;
pub mod utils;

use system_error::SystemError;

use self::utils::*;
use crate::libs::casting::DowncastArc;

use super::{PSO, PSOCK};
use crate::process::namespace::net_namespace::NetNamespace;
use crate::{
    filesystem::vfs::{
        inode_lifecycle::{InodeRetentionGuard, InodeRetentionKind},
        mount::MountFSInode,
        utils::{rsplit_path, DName},
        FileType, IndexNode, InodeId, InodeMode, VFS_MAX_FOLLOW_SYMLINK_TIMES,
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
use core::sync::atomic::{AtomicBool, Ordering};

/// SOL_SOCKET reuse options shared by AF_UNIX socket types. Unix address
/// binding does not use SO_REUSEADDR, but the setting is observable per socket.
#[derive(Debug, Default)]
pub(super) struct UnixReuseOptions {
    reuse_addr: AtomicBool,
}

impl UnixReuseOptions {
    pub(super) fn set(&self, option: PSO, value: &[u8]) -> Result<(), SystemError> {
        let source = value.get(..4).ok_or(SystemError::EINVAL)?;
        let mut bytes = [0u8; 4];
        bytes.copy_from_slice(source);
        let enabled = i32::from_ne_bytes(bytes) != 0;
        match option {
            PSO::REUSEADDR => {
                self.reuse_addr.store(enabled, Ordering::Relaxed);
                Ok(())
            }
            PSO::REUSEPORT if enabled => Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
            PSO::REUSEPORT => Ok(()),
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    pub(super) fn get(&self, option: PSO, value: &mut [u8]) -> Result<usize, SystemError> {
        let enabled = match option {
            PSO::REUSEADDR => self.reuse_addr.load(Ordering::Relaxed),
            PSO::REUSEPORT => false,
            _ => return Err(SystemError::ENOPROTOOPT),
        };
        Ok(super::common::write_i32_getsockopt(
            value,
            i32::from(enabled),
        ))
    }
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum UnixSocketType {
    Stream,
    Datagram,
    SeqPacket,
}

#[derive(Debug)]
pub(super) struct PathAddress {
    name: DName,
    identity: (usize, InodeId, u64),
    _retention: InodeRetentionGuard,
}

impl PathAddress {
    fn new(inode: Arc<dyn IndexNode>, name: DName) -> Result<Arc<Self>, SystemError> {
        let retention =
            InodeRetentionGuard::new(inode.clone(), InodeRetentionKind::OpenFileDescription)?;
        let mounted = inode
            .clone()
            .downcast_arc::<MountFSInode>()
            .ok_or(SystemError::EINVAL)?;
        let identity = mounted.inode_object_identity();
        Ok(Arc::new(Self {
            name,
            identity,
            _retention: retention,
        }))
    }
}

/// Cloneable address identity. This never owns an abstract-name reservation.
#[derive(Clone, Debug)]
pub(super) enum UnixEndpointBound {
    Path(Arc<PathAddress>),
    Abstract {
        nsid: usize,
        socket_type: UnixSocketType,
        name: Arc<[u8]>,
        incarnation: u64,
    },
}

/// The only owner of an abstract-name reservation follows the bound socket
/// through its state changes. Address snapshots and table keys never own it.
#[derive(Debug)]
pub(super) struct UnixBinding {
    pub(super) address: UnixEndpointBound,
    _abstract_handle: Option<Arc<AbstractHandle>>,
}

impl UnixBinding {
    fn path(address: Arc<PathAddress>) -> Self {
        Self {
            address: UnixEndpointBound::Path(address),
            _abstract_handle: None,
        }
    }

    pub(super) fn abstract_name(handle: Arc<AbstractHandle>) -> Self {
        let address = UnixEndpointBound::Abstract {
            nsid: handle.nsid(),
            socket_type: handle.socket_type(),
            name: handle.name(),
            incarnation: handle.incarnation(),
        };
        Self {
            address,
            _abstract_handle: Some(handle),
        }
    }

    pub(super) fn address(&self) -> UnixEndpointBound {
        self.address.clone()
    }

    /// The accepted half of a connection has the listener's address but does
    /// not become another owner of the listener's abstract-name reservation.
    pub(super) fn borrowed(address: UnixEndpointBound) -> Self {
        Self {
            address,
            _abstract_handle: None,
        }
    }
}

impl PartialEq for UnixEndpointBound {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (UnixEndpointBound::Path(path1), UnixEndpointBound::Path(path2)) => {
                path1.identity == path2.identity
            }
            (
                UnixEndpointBound::Abstract {
                    nsid: n1,
                    socket_type: t1,
                    name: a1,
                    incarnation: i1,
                },
                UnixEndpointBound::Abstract {
                    nsid: n2,
                    socket_type: t2,
                    name: a2,
                    incarnation: i2,
                },
            ) => (n1, t1, a1, i1) == (n2, t2, a2, i2),
            _ => false,
        }
    }
}

impl Eq for UnixEndpointBound {}

impl Ord for UnixEndpointBound {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        match (self, other) {
            (UnixEndpointBound::Path(path1), UnixEndpointBound::Path(path2)) => {
                path1.identity.cmp(&path2.identity)
            }
            (
                UnixEndpointBound::Abstract {
                    nsid: n1,
                    socket_type: t1,
                    name: a1,
                    incarnation: i1,
                },
                UnixEndpointBound::Abstract {
                    nsid: n2,
                    socket_type: t2,
                    name: a2,
                    incarnation: i2,
                },
            ) => (n1, t1, a1, i1).cmp(&(n2, t2, a2, i2)),
            (UnixEndpointBound::Path(_), UnixEndpointBound::Abstract { .. }) => {
                core::cmp::Ordering::Less
            }
            (UnixEndpointBound::Abstract { .. }, UnixEndpointBound::Path(_)) => {
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
                0u8.hash(state);
                path.identity.hash(state);
            }
            UnixEndpointBound::Abstract {
                nsid,
                socket_type,
                name,
                incarnation,
            } => {
                1u8.hash(state);
                (nsid, socket_type, name, incarnation).hash(state);
            }
        }
    }
}

impl From<UnixEndpointBound> for UnixEndpoint {
    fn from(endpoint: UnixEndpointBound) -> Self {
        match endpoint {
            UnixEndpointBound::Path(path) => UnixEndpoint::File(String::from(path.name.as_ref())),
            UnixEndpointBound::Abstract { name, .. } => UnixEndpoint::Abstract(name.to_vec()),
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
        socket_type: UnixSocketType,
    ) -> Result<UnixBinding, SystemError> {
        let bound = match self {
            Self::Unnamed => UnixBinding::abstract_name(
                netns
                    .unix_abstract_table()
                    .alloc_ephemeral_abstract_name(socket_type)?,
            ),
            Self::File(path) => {
                let (inode_begin, _) = crate::filesystem::vfs::utils::user_path_at(
                    &ProcessManager::current_pcb(),
                    crate::filesystem::vfs::fcntl::AtFlags::AT_FDCWD.bits(),
                    &path,
                )?;
                let (filename, parent_path) = rsplit_path(&path);
                let parent_inode = if let Some(parent) = parent_path {
                    inode_begin.lookup_follow_symlink(parent, VFS_MAX_FOLLOW_SYMLINK_TIMES)?
                } else {
                    inode_begin
                };
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
                UnixBinding::path(PathAddress::new(inode, DName::from(path))?)
            }
            Self::Abstract(name) => UnixBinding::abstract_name(
                netns
                    .unix_abstract_table()
                    .create_abstract_name_bytes(socket_type, &name)?,
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
        socket_type: UnixSocketType,
    ) -> Result<UnixEndpointBound, SystemError> {
        let bound = match self {
            Self::Unnamed => return Err(SystemError::EINVAL),
            Self::Abstract(name) => {
                UnixBinding::abstract_name(
                    netns
                        .unix_abstract_table()
                        .lookup_abstract_name_bytes(socket_type, name)?,
                )
                .address
            }
            Self::File(path) => {
                let (inode_begin, path) = crate::filesystem::vfs::utils::user_path_at(
                    &ProcessManager::current_pcb(),
                    crate::filesystem::vfs::fcntl::AtFlags::AT_FDCWD.bits(),
                    path.trim(),
                )?;
                let inode =
                    inode_begin.lookup_follow_symlink(&path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
                if inode.metadata()?.file_type != FileType::Socket {
                    return Err(SystemError::ECONNREFUSED);
                }
                // let inode = ProcessManager::current_mntns()
                //     .root_inode()
                //     .lookup_follow_symlink(path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;
                UnixEndpointBound::Path(PathAddress::new(inode, DName::from(path))?)
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
