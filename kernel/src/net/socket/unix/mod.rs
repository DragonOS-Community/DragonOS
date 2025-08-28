pub mod ns;
// pub mod seqpacket;
// pub mod datagram;
pub mod stream;

use crate::{
    filesystem::vfs::{IndexNode, InodeId, ROOT_INODE},
    net::{
        posix::SockAddrUn,
        socket::{unix::ns::AbstractUnixPath, AddressFamily, Socket},
    },
};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use system_error::SystemError;

use super::{Family, PSOCK};
pub struct Unix;

#[derive(Debug, Clone)]
pub enum UnixEndpoint {
    File(String),
    Abstract(AbstractUnixPath),
}

impl From<UnixEndpoint> for Box<SockAddrUn> {
    fn from(endpoint: UnixEndpoint) -> Self {
        use UnixEndpoint::*;
        let mut ret: Box<SockAddrUn> = unsafe { Box::new_zeroed().assume_init() };
        ret.sun_family = AddressFamily::Unix as u16;
        match endpoint {
            File(path) => {
                // TODO: handle path length

                ret.sun_path.copy_from_slice(&path.into_bytes());
            }
            Abstract(path) => {
                path.sun_path(&mut ret.sun_path);
            }
        };
        ret
    }
}

impl Family for Unix {
    fn socket(
        stype: PSOCK,
        _protocol: u32,
        is_nonblocking: bool,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        match stype {
            PSOCK::Stream | PSOCK::Datagram => stream::StreamSocket::new_inode(),
            // PSOCK::SeqPacket => seqpacket::SeqpacketSocket::new_inode(false),
            _ => Err(SystemError::EPROTONOSUPPORT),
        }
    }
}

impl Unix {
    pub fn new_pairs(
        socket_type: PSOCK,
        is_nonblocking: bool,
    ) -> Result<(Arc<dyn IndexNode>, Arc<dyn IndexNode>), SystemError> {
        // log::debug!("socket_type {:?}", socket_type);
        match socket_type {
            // PSOCK::SeqPacket => seqpacket::SeqpacketSocket::new_pairs(is_nonblocking),
            PSOCK::Stream | PSOCK::Datagram => stream::StreamSocket::new_pairs(),
            _ => todo!(),
        }
    }
}
