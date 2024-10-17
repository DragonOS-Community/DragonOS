use anyhow::bail;
use std::{
    ffi::CString,
    net::{self},
};

use crate::error;

///# Config结构体
/// 记录ping指令的一些参数值
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

///# 目标地址ip结构体
/// ip负责提供给socket使用
/// raw负责打印输出
#[derive(Debug, Clone)]
pub struct IpAddress {
    pub ip: net::IpAddr,
    pub raw: String,
}

impl IpAddress {
    pub fn parse(host: &str) -> anyhow::Result<Self> {
        let raw = String::from(host);
        let opt = host.parse::<net::IpAddr>().ok();
        match opt {
            Some(ip) => Ok(Self { ip, raw }),
            None => {
                bail!(error::PingError::InvalidConfig(
                    "Invalid Address".to_string()
                ));
            }
        }
    }
}
