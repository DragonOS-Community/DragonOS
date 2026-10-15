//! Decode an IP bind address without applying connect's AF_UNSPEC semantics.

use smoltcp::wire::{IpAddress, IpEndpoint, IpVersion};
use system_error::SystemError;

use crate::net::posix::{SockAddr, SockAddrIn, MAX_SOCKADDR_LEN};
use crate::net::socket::{endpoint::Endpoint, AddressFamily};
use crate::syscall::user_buffer::UserBuffer;

/// Linux accepts the original RFC 2133 IPv6 layout without the scope-id tail.
const IPV6_MIN_BIND_LEN: usize = 24;

pub(crate) fn endpoint_from_user(
    addr: *const SockAddr,
    len: u32,
    version: IpVersion,
) -> Result<Endpoint, SystemError> {
    let mut endpoint = endpoint_from_user_preserving_mapped(addr, len, version)?;
    if let Endpoint::Ip(endpoint) = &mut endpoint {
        if let IpAddress::Ipv6(address) = endpoint.addr {
            if let Some(mapped) = address.to_ipv4_mapped() {
                endpoint.addr = IpAddress::Ipv4(mapped);
            }
        }
    }
    Ok(endpoint)
}

/// TCP keeps mapped input until its synchronized IPV6_V6ONLY check.
pub(crate) fn endpoint_from_user_preserving_mapped(
    addr: *const SockAddr,
    len: u32,
    version: IpVersion,
) -> Result<Endpoint, SystemError> {
    if len == 0 || len > MAX_SOCKADDR_LEN {
        return Err(SystemError::EINVAL);
    }
    // Copy the entire declared address before protocol validation, as Linux's
    // move_addr_to_kernel does. Never construct a Rust slice over user memory.
    let mut snapshot = [0u8; MAX_SOCKADDR_LEN as usize];
    UserBuffer::new_protected(addr as *mut u8, len as usize, true)?
        .read_from_user(0, &mut snapshot[..len as usize])?;
    let minimum = match version {
        IpVersion::Ipv4 => core::mem::size_of::<SockAddrIn>(),
        IpVersion::Ipv6 => IPV6_MIN_BIND_LEN,
    };
    if (len as usize) < minimum {
        return Err(SystemError::EINVAL);
    }
    let family = u16::from_ne_bytes([snapshot[0], snapshot[1]]);
    let port = u16::from_be_bytes([snapshot[2], snapshot[3]]);
    let address = match version {
        IpVersion::Ipv4 => {
            let address = IpAddress::v4(snapshot[4], snapshot[5], snapshot[6], snapshot[7]);
            if family != AddressFamily::INet as u16
                && !(family == AddressFamily::Unspecified as u16 && address.is_unspecified())
            {
                return Err(SystemError::EAFNOSUPPORT);
            }
            address
        }
        IpVersion::Ipv6 => {
            if family != AddressFamily::INet6 as u16 {
                return Err(SystemError::EAFNOSUPPORT);
            }
            return Ok(Endpoint::Ip(
                crate::net::socket::endpoint::InetEndpoint::from_ipv6_sockaddr(
                    &snapshot[..len as usize],
                )?,
            ));
        }
    };
    Ok(Endpoint::ip(IpEndpoint::new(address, port)))
}
