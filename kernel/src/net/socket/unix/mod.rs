pub mod ns;
pub mod ring_buffer;
pub mod stream;

use super::PSOCK;
use crate::{
    filesystem::vfs::{syscall::InodeMode, utils::rsplit_path, VFS_MAX_FOLLOW_SYMLINK_TIMES},
    net::socket::{
        endpoint::Endpoint,
        unix::{ns::AbstractHandle, stream::UnixStreamSocket},
        Socket,
    },
    process::ProcessManager,
};
use alloc::string::String;
use alloc::sync::Arc;
use core::hash::Hash;
use system_error::SystemError;

#[derive(Debug, Clone)]
pub enum UnixEndpoint {
    File(String),
    Abstract(String),
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
    Path(Arc<str>),
    Abstract(Arc<AbstractHandle>),
}

impl PartialEq for UnixEndpointBound {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (UnixEndpointBound::Path(path1), UnixEndpointBound::Path(path2)) => path1 == path2,
            (UnixEndpointBound::Abstract(handle1), UnixEndpointBound::Abstract(handle2)) => {
                handle1.name() == handle2.name()
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
                handle1.name().cmp(&handle2.name())
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
                handle.name().hash(state);
            }
        }
    }
}

impl From<UnixEndpointBound> for UnixEndpoint {
    fn from(endpoint: UnixEndpointBound) -> Self {
        match endpoint {
            UnixEndpointBound::Path(path) => UnixEndpoint::File(String::from(&*path)),
            UnixEndpointBound::Abstract(handle) => {
                UnixEndpoint::Abstract(String::from_utf8_lossy(&handle.name()).into_owned())
            }
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
    pub(super) fn bind(self) -> Result<UnixEndpointBound, SystemError> {
        let bound = match self {
            Self::Unnamed => UnixEndpointBound::Abstract(ns::alloc_ephemeral_abstract_name()?),
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
                let inode = parent_inode.create(
                    filename,
                    crate::filesystem::vfs::FileType::Socket,
                    InodeMode::S_IWUSR,
                )?;
                UnixEndpointBound::Path(Arc::from(inode.absolute_path()?.as_str()))
            }
            Self::Abstract(name) => UnixEndpointBound::Abstract(ns::create_abstract_name(name)?),
        };

        Ok(bound)
    }

    pub(super) fn bind_unnamed(&self) -> Result<(), SystemError> {
        if matches!(self, UnixEndpoint::Unnamed) {
            return Ok(());
        }
        Err(SystemError::EINVAL)
    }

    pub(super) fn connect(&self) -> Result<UnixEndpointBound, SystemError> {
        let bound = match self {
            Self::Unnamed => return Err(SystemError::EINVAL),
            Self::Abstract(name) => {
                UnixEndpointBound::Abstract(ns::lookup_abstract_name(name.as_bytes())?)
            }
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
                UnixEndpointBound::Path(Arc::from(abso_path.as_str()))
            }
        };

        Ok(bound)
    }
}

pub fn create_unix_socket(
    socket_type: PSOCK,
    is_nonblocking: bool,
) -> Result<Arc<dyn Socket>, SystemError> {
    let socket = match socket_type {
        PSOCK::Stream => UnixStreamSocket::new(is_nonblocking, false),
        PSOCK::Packet => UnixStreamSocket::new(is_nonblocking, true),
        _ => {
            return Err(SystemError::ESOCKTNOSUPPORT);
        }
    };
    Ok(socket)
}
