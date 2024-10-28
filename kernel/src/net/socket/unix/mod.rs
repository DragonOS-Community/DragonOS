pub mod ns;
pub(crate) mod seqpacket;
pub mod stream;
use crate::{filesystem::vfs::InodeId, libs::rwlock::RwLock, net::socket::*};
use alloc::sync::Arc;
use hashbrown::HashMap;
use system_error::SystemError::{self, *};
pub struct Unix;

lazy_static! {
    pub static ref INODE_MAP: RwLock<HashMap<InodeId, Endpoint>> = RwLock::new(HashMap::new());
}

fn create_unix_socket(sock_type: PSOCK) -> Result<Arc<Inode>, SystemError> {
    match sock_type {
        PSOCK::Stream | PSOCK::Datagram => stream::StreamSocket::new_inode(),
        PSOCK::SeqPacket => seqpacket::SeqpacketSocket::new_inode(false),
        _ => Err(EPROTONOSUPPORT),
    }
}

impl family::Family for Unix {
    fn socket(stype: PSOCK, _protocol: u32) -> Result<Arc<Inode>, SystemError> {
        let socket = create_unix_socket(stype)?;
        Ok(socket)
    }
}

impl Unix {
    pub fn new_pairs(socket_type: PSOCK) -> Result<(Arc<Inode>, Arc<Inode>), SystemError> {
        // log::debug!("socket_type {:?}", socket_type);
        match socket_type {
            PSOCK::SeqPacket => seqpacket::SeqpacketSocket::new_pairs(),
            PSOCK::Stream | PSOCK::Datagram => stream::StreamSocket::new_pairs(),
            _ => todo!(),
        }
    }
}
