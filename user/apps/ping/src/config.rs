use std::net::{self};
use anyhow::bail;

use crate::error;

#[derive(Debug, Clone)]
pub struct Config {
    pub count: u16,
    pub packet_size: usize,
    pub ttl: u32,
    pub timeout: u64,
    pub interval: u64,
    pub id: u16,
    pub sequence: u16,
    pub address: IpAddress,
}

#[derive(Debug, Clone)]
pub struct IpAddress {
    pub ip: net::IpAddr,
    pub raw: String,
}

impl IpAddress {
    pub fn parse(host: &str) -> anyhow::Result<Self>{
        println!("{:?}", host);
        let raw = String::from(host);
        println!("{:?}", raw);
        let opt = host.parse::<net::IpAddr>().ok();
        match opt {
            Some(ip) => {
                Ok(Self{
                    ip,
                    raw,
                })
            }
            None => {
                bail!(error::PingError::InvalidConfig("Invalid Address".to_string()));
            }
        }

    }
}