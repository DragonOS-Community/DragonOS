use clap::{arg, command, Parser};
use rand::random;

use crate::config::{Config, IpAddress};

/// # Args结构体
/// 使用clap库对命令行输入进行pasing，产生参数配置
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    // Count of ping times
    #[arg(short, default_value_t = 4)]
    count: u16,

    // Ping packet size
    #[arg(short = 's', default_value_t = 64)]
    packet_size: usize,

    // Ping ttl
    #[arg(short = 't', default_value_t = 64)]
    ttl: u32,

    // Ping timeout seconds
    #[arg(short = 'w', default_value_t = 1)]
    timeout: u64,

    // Ping interval duration milliseconds
    #[arg(short = 'i', default_value_t = 1000)]
    interval: u64,

    // Ping destination, ip or domain
    #[arg(value_parser=IpAddress::parse)]
    destination: IpAddress,
}

impl Args {
    /// # 将Args结构体转换为config结构体
    pub fn as_config(&self) -> Config {
        Config {
            count: self.count,
            packet_size: self.packet_size,
            ttl: self.ttl,
            timeout: self.timeout,
            interval: self.interval,
            id: random::<u16>(),
            sequence: 1,
            address: self.destination.clone(),
        }
    }
}
