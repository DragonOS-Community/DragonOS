#![allow(dead_code)]

#[derive(Debug, Clone, thiserror::Error)]
pub enum PingError {
    #[error("invaild config")]
    InvalidConfig(String),

    #[error("invaild packet")]
    InvalidPacket,
    
}