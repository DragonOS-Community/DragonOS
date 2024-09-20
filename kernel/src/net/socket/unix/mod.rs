mod stream;
pub(crate) mod seqpacket;
use crate::{filesystem::vfs::InodeId, libs::rwlock::RwLock, net::socket::*};
use hashbrown::HashMap;
use system_error::SystemError::{self, *};
use alloc::sync::Arc;
pub struct Unix;

lazy_static!{
    pub static ref INODE_MAP: RwLock<HashMap<InodeId, Endpoint>> = RwLock::new(HashMap::new());
}

fn create_unix_socket(
    sock_type: Type,
) -> Result<Arc<Inode>, SystemError> {
    match sock_type {
        Type::Stream | Type::Datagram => {
            stream::StreamSocket::new_inode()
        },
        Type::SeqPacket => {
            // Ok(seqpacket::SeqpacketSocket::new(false))
            seqpacket::SeqpacketSocket::new_inode(false)
        },
        _ => Err(EPROTONOSUPPORT),
    }
}

impl family::Family for Unix {
    fn socket(stype: Type, _protocol: u32) -> Result<Arc<Inode>, SystemError> {
        let socket = create_unix_socket(stype)?;
        // Ok(Inode::new(socket))
        Ok(socket)
    }
}

impl Unix {
    pub fn new_pairs(socket_type:Type) ->Result<(Arc<Inode>,Arc<Inode>),SystemError>{
        log::debug!("socket_type {:?}",socket_type);
        match socket_type {
            Type::SeqPacket |Type::Datagram=>seqpacket::SeqpacketSocket::new_pairs(),
            _=>todo!()
        }
    }
}