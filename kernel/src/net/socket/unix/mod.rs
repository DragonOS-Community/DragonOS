mod stream;
mod seqpacket;
use crate::net::socket::*;
use system_error::SystemError::{self, *};
use alloc::sync::Arc;
pub struct Unix;

fn create_unix_socket(
    sock_type: Type,
) -> Result<Arc<dyn Socket>, SystemError> {
    match sock_type {
        Type::Stream => {
            Ok(stream::StreamSocket::new())
        },
        Type::SeqPacket =>{
            Ok(seqpacket::SeqpacketSocket::new(false))
        }
        _ => {
            Err(EPROTONOSUPPORT)
        }
    }
}

impl family::Family for Unix {
    fn socket(stype: Type, _protocol: u32) -> Result<Arc<Inode>, SystemError> {
        let socket = create_unix_socket(stype)?;
        Ok(Inode::new(socket))
    }
}