mod stream;
pub(crate) mod seqpacket;
use crate::{libs::rwlock::RwLock, net::socket::*};
use alloc::sync::Arc;
use hashbrown::HashMap;
use system_error::SystemError::{self, *};
pub struct Unix;

fn create_unix_socket(sock_type: Type) -> Result<Arc<dyn Socket>, SystemError> {
    match sock_type {
        Type::Datagram => {
            Ok(stream::StreamSocket::new())
        },
        Type::SeqPacket =>{
            Ok(seqpacket::SeqpacketSocket::new(false))
        }
        _ => {
            Err(EPROTONOSUPPORT)
        }
        _ => Err(EPROTONOSUPPORT),
    }
}

impl family::Family for Unix {
    fn socket(stype: Type, _protocol: u32) -> Result<Arc<Inode>, SystemError> {
        let socket = create_unix_socket(stype)?;
        Ok(Inode::new(socket))
    }
}

impl Unix {
    pub fn new_pairs(socket_type:Type) ->Result<(Arc<Inode>,Arc<Inode>),SystemError>{
        match socket_type {
            Type::SeqPacket=>seqpacket::SeqpacketSocket::new_pairs(),
            _=>todo!()
        }
    }
}

static PNODE_TABLE: PnodeTable = PnodeTable::new();

pub struct PnodeTable {
    unix_sockets: RwLock<HashMap<usize, Arc<Inode>>>,
}

impl PnodeTable {
    pub fn new() -> Self {
        Self { unix_sockets: RwLock::new(HashMap::new()) }
    }

    pub fn add_entry(&self, inode_number: &usize, snode: Arc<Inode>) -> Result<(), SystemError>{
        let mut sockets = self.unix_sockets.write();
        if sockets.contains_key(inode_number) {
            return Err(SystemError::EINVAL); 
        }   
        sockets.insert(inode_number, snode);
        Ok(())
    }

    pub fn delete_entry(&self, inode_number: &usize) -> Result<(), SystemError>{
        let mut sockets = self.unix_sockets.write();
        if sockets.contains_key(inode_number) {
            sockets.remove(inode_socket);
            Ok(())
        }
        return Err(SystemError::EINVAL);
    }

    pub fn get_entry(&self, inode_number: &usize) -> Arc<Inode>{
        return self.unix_sockets.read().get(inode_number)
    }
}