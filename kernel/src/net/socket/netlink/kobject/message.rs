use alloc::vec::Vec;

use crate::net::socket::netlink::table::MulticastMessage;

#[derive(Debug, Clone)]
pub struct KobjectUeventMessage {
    bytes: Vec<u8>,
}

impl KobjectUeventMessage {
    pub fn new(payload: &[u8]) -> Self {
        Self {
            bytes: payload.to_vec(),
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl MulticastMessage for KobjectUeventMessage {}
