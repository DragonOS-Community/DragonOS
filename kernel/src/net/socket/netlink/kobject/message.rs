use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::net::socket::netlink::table::MulticastMessage;

#[derive(Debug, Clone)]
pub struct KobjectUeventMessage {
    bytes: Arc<Vec<u8>>,
}

impl KobjectUeventMessage {
    pub fn new(payload: &[u8]) -> Self {
        Self {
            bytes: Arc::new(payload.to_vec()),
        }
    }

    pub fn try_new(payload: &[u8]) -> Result<Self, SystemError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve(payload.len())
            .map_err(|_| SystemError::ENOMEM)?;
        bytes.extend_from_slice(payload);
        Self::try_from_vec(bytes)
    }

    pub fn from_vec(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Arc::new(bytes),
        }
    }

    pub fn try_from_vec(bytes: Vec<u8>) -> Result<Self, SystemError> {
        Ok(Self {
            bytes: Arc::try_new(bytes).map_err(|_| SystemError::ENOMEM)?,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_ref()
    }
}

impl MulticastMessage for KobjectUeventMessage {}
