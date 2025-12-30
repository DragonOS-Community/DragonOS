pub(super) mod datagram_common;

use crate::net::socket::{
    self, inet::syscall::create_inet_socket, netlink::create_netlink_socket, packet::PacketSocket,
    unix::create_unix_socket, Socket,
};
use alloc::sync::Arc;
use smoltcp::wire::{IpAddress, IpVersion};
use system_error::SystemError;

/// IPv4 头最小长度
pub const IPV4_MIN_HEADER_LEN: usize = 20;
/// IPv6 头长度
pub const IPV6_HEADER_LEN: usize = 40;

/// 从 IP 头提取源地址
///
/// # 参数
/// - `data`: IP 数据包（包含 IP 头）
/// - `ip_version`: IP 版本
///
/// # 返回
/// - `Ok(IpAddress)`: 成功提取的源地址
/// - `Err(SystemError::EINVAL)`: 数据包长度不足
pub fn extract_src_addr_from_ip_header(
    data: &[u8],
    ip_version: IpVersion,
) -> Result<IpAddress, SystemError> {
    match ip_version {
        IpVersion::Ipv4 => {
            if data.len() < IPV4_MIN_HEADER_LEN {
                return Err(SystemError::EINVAL);
            }
            // IPv4 源地址在字节 12-15
            Ok(IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(
                data[12], data[13], data[14], data[15],
            )))
        }
        IpVersion::Ipv6 => {
            if data.len() < IPV6_HEADER_LEN {
                return Err(SystemError::EINVAL);
            }
            // IPv6 源地址在字节 8-23
            let src_bytes: [u8; 16] = data[8..24].try_into().map_err(|_| SystemError::EINVAL)?;
            Ok(IpAddress::Ipv6(smoltcp::wire::Ipv6Address::new(
                u16::from_be_bytes([src_bytes[0], src_bytes[1]]),
                u16::from_be_bytes([src_bytes[2], src_bytes[3]]),
                u16::from_be_bytes([src_bytes[4], src_bytes[5]]),
                u16::from_be_bytes([src_bytes[6], src_bytes[7]]),
                u16::from_be_bytes([src_bytes[8], src_bytes[9]]),
                u16::from_be_bytes([src_bytes[10], src_bytes[11]]),
                u16::from_be_bytes([src_bytes[12], src_bytes[13]]),
                u16::from_be_bytes([src_bytes[14], src_bytes[15]]),
            )))
        }
    }
}

/// 从 IP 头提取目的地址
///
/// 该函数用于实现 Linux raw socket 的 bind(2) 目的地址过滤等语义。
///
/// # 参数
/// - `data`: IP 数据包（包含 IP 头）
/// - `ip_version`: IP 版本
///
/// # 返回
/// - `Some(IpAddress)`: 成功提取的目的地址
/// - `None`: 数据包长度不足
pub fn extract_dst_addr_from_ip_header(data: &[u8], ip_version: IpVersion) -> Option<IpAddress> {
    match ip_version {
        IpVersion::Ipv4 => {
            if data.len() < IPV4_MIN_HEADER_LEN {
                return None;
            }
            // IPv4 目的地址在字节 16-19
            Some(IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(
                data[16], data[17], data[18], data[19],
            )))
        }
        IpVersion::Ipv6 => {
            if data.len() < IPV6_HEADER_LEN {
                return None;
            }
            // IPv6 目的地址在字节 24-39
            let dst_bytes: [u8; 16] = data[24..40].try_into().ok()?;
            Some(IpAddress::Ipv6(smoltcp::wire::Ipv6Address::new(
                u16::from_be_bytes([dst_bytes[0], dst_bytes[1]]),
                u16::from_be_bytes([dst_bytes[2], dst_bytes[3]]),
                u16::from_be_bytes([dst_bytes[4], dst_bytes[5]]),
                u16::from_be_bytes([dst_bytes[6], dst_bytes[7]]),
                u16::from_be_bytes([dst_bytes[8], dst_bytes[9]]),
                u16::from_be_bytes([dst_bytes[10], dst_bytes[11]]),
                u16::from_be_bytes([dst_bytes[12], dst_bytes[13]]),
                u16::from_be_bytes([dst_bytes[14], dst_bytes[15]]),
            )))
        }
    }
}

pub fn create_socket(
    family: socket::AddressFamily,
    socket_type: socket::PSOCK,
    protocol: u32,
    is_nonblock: bool,
    _is_close_on_exec: bool,
) -> Result<Arc<dyn Socket>, SystemError> {
    // log::info!("Creating socket: {:?}, {:?}, {:?}", family, socket_type, protocol);
    type AF = socket::AddressFamily;
    let inode = match family {
        AF::INet => create_inet_socket(
            smoltcp::wire::IpVersion::Ipv4,
            socket_type,
            smoltcp::wire::IpProtocol::from(protocol as u8),
            is_nonblock,
        )?,
        AF::INet6 => create_inet_socket(
            smoltcp::wire::IpVersion::Ipv6,
            socket_type,
            smoltcp::wire::IpProtocol::from(protocol as u8),
            is_nonblock,
        )?,
        AF::Unix => create_unix_socket(socket_type, is_nonblock)?,
        AF::Netlink => create_netlink_socket(socket_type, protocol, is_nonblock)?,
        AF::Packet => {
            // AF_PACKET: protocol 是网络字节序的以太网协议类型
            // 常见值: ETH_P_ALL=0x0003, ETH_P_IP=0x0800, ETH_P_ARP=0x0806
            let eth_protocol = (protocol as u16).to_be();
            PacketSocket::new(socket_type, eth_protocol, is_nonblock)?
        }
        _ => {
            log::warn!("unsupport address family");
            return Err(SystemError::EAFNOSUPPORT);
        }
    };
    // inode.set_close_on_exec(is_close_on_exec);
    return Ok(inode);
}
