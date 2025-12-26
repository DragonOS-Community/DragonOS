//
// posix.rs 记录了系统调用时用到的结构
//

/// 通用地址结构体的最大大小（参考 Linux 的 sockaddr_storage）
/// 用于限制所有地址家族的 addrlen 上限，防止内核读取越界内存
///
/// 参考 Linux kernel net/socket.c:move_addr_to_kernel()
/// if (ulen < 0 || ulen > sizeof(struct sockaddr_storage))
///     return -EINVAL;
pub const MAX_SOCKADDR_LEN: u32 = 128;

bitflags::bitflags! {
    // #[derive(PartialEq, Eq, Debug, Clone, Copy)]
    pub struct PosixArgsSocketType: u32 {
        const DGRAM     = 1;    // 0b0000_0001
        const STREAM    = 2;    // 0b0000_0010
        const RAW       = 3;    // 0b0000_0011
        const RDM       = 4;    // 0b0000_0100
        const SEQPACKET = 5;    // 0b0000_0101
        const DCCP      = 6;    // 0b0000_0110
        const PACKET    = 10;   // 0b0000_1010

        const NONBLOCK  = crate::filesystem::vfs::file::FileFlags::O_NONBLOCK.bits();
        const CLOEXEC   = crate::filesystem::vfs::file::FileFlags::O_CLOEXEC.bits();
    }
}

impl PosixArgsSocketType {
    #[inline(always)]
    pub fn types(&self) -> PosixArgsSocketType {
        PosixArgsSocketType::from_bits(self.bits() & 0b_1111).unwrap()
    }

    #[inline(always)]
    pub fn is_nonblock(&self) -> bool {
        self.contains(PosixArgsSocketType::NONBLOCK)
    }

    #[inline(always)]
    pub fn is_cloexec(&self) -> bool {
        self.contains(PosixArgsSocketType::CLOEXEC)
    }
}

use super::socket::{endpoint::Endpoint, AddressFamily};
use crate::net::socket::netlink::addr::{multicast::GroupIdSet, NetlinkSocketAddr};
use crate::net::socket::unix::UnixEndpoint;
use crate::syscall::user_access::UserBufferReader;
use alloc::string::ToString;
use alloc::vec::Vec;
use system_error::SystemError;

// 参考资料： https://pubs.opengroup.org/onlinepubs/9699919799/basedefs/netinet_in.h.html#tag_13_32
/// struct sockaddr_in for IPv4 addresses
/// This matches the C structure layout
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SockAddrIn {
    pub sin_family: u16,   // AF_INET = 2
    pub sin_port: u16,     // Port number (network byte order)
    pub sin_addr: u32,     // IPv4 address (network byte order)
    pub sin_zero: [u8; 8], // Padding to match struct sockaddr size (total 16 bytes)
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockAddrUn {
    pub sun_family: u16,
    pub sun_path: [u8; 108],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockAddrLl {
    pub sll_family: u16,
    pub sll_protocol: u16,
    pub sll_ifindex: u32,
    pub sll_hatype: u16,
    pub sll_pkttype: u8,
    pub sll_halen: u8,
    pub sll_addr: [u8; 8],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockAddrNl {
    pub nl_family: AddressFamily,
    pub nl_pad: u16,
    pub nl_pid: u32,
    pub nl_groups: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockAddrPlaceholder {
    pub family: u16,
    pub data: [u8; 14],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union SockAddr {
    pub family: u16,
    pub addr_in: SockAddrIn,
    pub addr_un: SockAddrUn,
    pub addr_ll: SockAddrLl,
    pub addr_nl: SockAddrNl,
    pub addr_ph: SockAddrPlaceholder,
}

impl From<smoltcp::wire::IpEndpoint> for SockAddr {
    fn from(value: smoltcp::wire::IpEndpoint) -> Self {
        match value.addr {
            smoltcp::wire::IpAddress::Ipv4(ipv4_addr) => Self {
                addr_in: SockAddrIn {
                    sin_family: AddressFamily::INet as u16,
                    sin_port: value.port.to_be(),
                    sin_addr: ipv4_addr.to_bits().to_be(),
                    sin_zero: Default::default(),
                },
            },
            smoltcp::wire::IpAddress::Ipv6(_ipv6_addr) => todo!(),
        }
    }
}

impl From<UnixEndpoint> for SockAddr {
    fn from(value: UnixEndpoint) -> Self {
        let mut sun_path = [0u8; 108];

        match value {
            UnixEndpoint::File(path) => {
                let path_bytes = path.as_bytes();
                let copy_len = core::cmp::min(path_bytes.len(), 107); // 留一个字节给null终止符
                sun_path[..copy_len].copy_from_slice(&path_bytes[..copy_len]);
                // 确保以null结尾
                sun_path[copy_len] = 0;
            }
            UnixEndpoint::Abstract(name) => {
                // Abstract namespace以null字节开头
                sun_path[0] = 0;
                let copy_len = core::cmp::min(name.len(), 107);
                sun_path[1..1 + copy_len].copy_from_slice(&name[..copy_len]);
            }
            UnixEndpoint::Unnamed => {
                // Unnamed socket，所有字节保持为0
            }
        }

        SockAddr {
            addr_un: SockAddrUn {
                sun_family: AddressFamily::Unix as u16,
                sun_path,
            },
        }
    }
}

impl From<NetlinkSocketAddr> for SockAddr {
    fn from(value: NetlinkSocketAddr) -> Self {
        SockAddr {
            addr_nl: SockAddrNl {
                nl_family: AddressFamily::Netlink,
                nl_pad: 0,
                nl_pid: value.port(),
                nl_groups: value.groups().as_u32(),
            },
        }
    }
}

impl From<Endpoint> for SockAddr {
    fn from(value: Endpoint) -> Self {
        match value {
            Endpoint::LinkLayer(_link_layer_endpoint) => todo!(),
            Endpoint::Ip(endpoint) => Self::from(endpoint),
            Endpoint::Unix(unix_endpoint) => Self::from(unix_endpoint),
            Endpoint::Netlink(netlink_addr) => Self::from(netlink_addr),
        }
    }
}

impl SockAddr {
    /// @brief 把用户传入的SockAddr转换为Endpoint结构体
    pub fn to_endpoint(addr: *const SockAddr, len: u32) -> Result<Endpoint, SystemError> {
        // 统一的上限检查：防止 addrlen 过大导致内核读取越界内存
        // 参考 Linux kernel net/socket.c:move_addr_to_kernel()
        if len > MAX_SOCKADDR_LEN {
            log::error!(
                "addr_len {} exceeds MAX_SOCKADDR_LEN {}",
                len,
                MAX_SOCKADDR_LEN
            );
            return Err(SystemError::EINVAL);
        }

        // 至少需要包含 sa_family
        if len < size_of::<u16>() as u32 {
            log::error!("addr_len {} < sizeof(sa_family_t)", len);
            return Err(SystemError::EINVAL);
        }

        // 使用 UserBufferReader 安全地读取用户空间数据
        // buffer_protected 会使用异常表保护访问，防止用户地址缺页导致内核崩溃
        let reader = UserBufferReader::new(addr as *const u8, len as usize, true)?;

        // 先读取 sa_family 确定地址家族
        let family = reader.buffer_protected(0)?.read_one::<u16>(0)?;
        let family = AddressFamily::try_from(family)?;

        match family {
            AddressFamily::INet => {
                // 下限检查：至少需要包含完整的 sockaddr_in 结构体
                if len < size_of::<SockAddrIn>() as u32 {
                    log::error!(
                        "len {} < sizeof(sockaddr_in) {}",
                        len,
                        size_of::<SockAddrIn>()
                    );
                    return Err(SystemError::EINVAL);
                }

                let addr_in = reader.buffer_protected(0)?.read_one::<SockAddrIn>(0)?;

                use smoltcp::wire;
                let ip: wire::IpAddress = wire::IpAddress::from(wire::Ipv4Address::from_bits(
                    u32::from_be(addr_in.sin_addr),
                ));
                let port = u16::from_be(addr_in.sin_port);

                return Ok(Endpoint::Ip(wire::IpEndpoint::new(ip, port)));
            }
            // AddressFamily::INet6 => {
            //     // IPv6 support to be implemented
            // }
            AddressFamily::Unix => {
                // 在这里并没有分配抽象地址或者创建文件系统节点，这里只是简单的获取，等到bind时再创建

                // Linux 语义：addrlen 过长应返回 EINVAL。
                // 参考 Linux kernel net/unix/af_unix.c:unix_validate_addr()
                // 注：虽然统一上限检查 (MAX_SOCKADDR_LEN) 已覆盖此场景，但保留此检查以保持与 Linux
                // unix_validate_addr() 的语义精确性 (addr_len > sizeof(struct sockaddr_un))
                if len > size_of::<SockAddrUn>() as u32 {
                    return Err(SystemError::EINVAL);
                }

                // 至少需要包含 sa_family_t（与 Linux unix_validate_addr 行为一致）
                // 已在函数开头检查过

                // Linux semantics: addrlen may be shorter than sizeof(sockaddr_un).
                // Only the bytes within addrlen are visible; do not attempt to read a full
                // SockAddrUn when len is short.
                let mut sun_path_buf = [0u8; 108];
                let sun_path_len = (len as usize)
                    .saturating_sub(size_of::<u16>())
                    .min(sun_path_buf.len());

                if sun_path_len != 0 {
                    let ub = reader.buffer_protected(size_of::<u16>())?;
                    ub.read_from_user(0, &mut sun_path_buf[..sun_path_len])?;
                }

                let sun_path = &sun_path_buf[..sun_path_len];

                if sun_path.is_empty() {
                    // Linux semantics: bind(addrlen == sizeof(sa_family_t)) triggers autobind
                    // to an abstract address.
                    return Ok(Endpoint::Unix(UnixEndpoint::Unnamed));
                }

                if sun_path[0] == 0 {
                    // 抽象地址空间，与文件系统没有关系
                    // Linux semantics: abstract names are binary and length-delimited (may
                    // contain embedded NULs). Do not treat them as C strings.
                    if sun_path_len <= 1 {
                        // A lone leading NUL (no name bytes) behaves like an unnamed bind.
                        return Ok(Endpoint::Unix(UnixEndpoint::Unnamed));
                    }

                    let name: Vec<u8> = sun_path[1..sun_path_len].to_vec();
                    return Ok(Endpoint::Unix(UnixEndpoint::Abstract(name)));
                }

                // Filesystem pathname sockets: respect addrlen and stop at the first NUL if any.
                let path_bytes = match sun_path.iter().position(|&b| b == 0) {
                    Some(nul) => &sun_path[..nul],
                    None => sun_path,
                };
                let path = core::str::from_utf8(path_bytes).map_err(|_| SystemError::EINVAL)?;

                // let (inode_begin, path) = crate::filesystem::vfs::utils::user_path_at(
                //     &ProcessManager::current_pcb(),
                //     crate::filesystem::vfs::fcntl::AtFlags::AT_FDCWD.bits(),
                //     path.trim(),
                // )?;
                // let _inode =
                //     inode_begin.lookup_follow_symlink(&path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

                return Ok(Endpoint::Unix(UnixEndpoint::File(path.to_string())));
            }
            AddressFamily::Netlink => {
                // 下限检查：至少需要包含完整的 sockaddr_nl 结构体
                if len < size_of::<SockAddrNl>() as u32 {
                    log::error!(
                        "len {} < sizeof(sockaddr_nl) {}",
                        len,
                        size_of::<SockAddrNl>()
                    );
                    return Err(SystemError::EINVAL);
                }

                let addr_nl = reader.buffer_protected(0)?.read_one::<SockAddrNl>(0)?;
                let nl_pid = addr_nl.nl_pid;
                let nl_groups = addr_nl.nl_groups;

                Ok(Endpoint::Netlink(NetlinkSocketAddr::new(
                    nl_pid,
                    GroupIdSet::new(nl_groups),
                )))
            }
            _ => {
                log::warn!("not support address family {:?}", family);
                return Err(SystemError::EINVAL);
            }
        }
    }

    /// @brief 获取地址长度
    pub fn len(&self) -> Result<u32, SystemError> {
        match AddressFamily::try_from(unsafe { self.family })? {
            AddressFamily::INet => Ok(core::mem::size_of::<SockAddrIn>()),
            AddressFamily::Packet => Ok(core::mem::size_of::<SockAddrLl>()),
            AddressFamily::Netlink => Ok(core::mem::size_of::<SockAddrNl>()),
            AddressFamily::Unix => Ok(core::mem::size_of::<SockAddrUn>()),
            _ => Err(SystemError::EINVAL),
        }
        .map(|x| x as u32)
    }

    pub unsafe fn is_empty(&self) -> bool {
        self.family == 0 && self.addr_ph.data == [0; 14]
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MsgHdr {
    /// 指向一个SockAddr结构体的指针
    pub msg_name: *mut SockAddr,
    /// SockAddr结构体的大小
    pub msg_namelen: u32,
    /// Padding to keep the same layout as Linux `struct msghdr` on 64-bit.
    #[cfg(target_pointer_width = "64")]
    pub _pad0: u32,
    /// scatter/gather array
    pub msg_iov: *mut crate::filesystem::vfs::iov::IoVec,
    /// elements in msg_iov
    pub msg_iovlen: usize,
    /// 辅助数据
    pub msg_control: *mut u8,
    /// 辅助数据长度
    pub msg_controllen: usize,
    /// 接收到的消息的标志
    pub msg_flags: i32,
    /// Padding to keep the same layout as Linux `struct msghdr` on 64-bit.
    #[cfg(target_pointer_width = "64")]
    pub _pad1: i32,
}

// TODO: 从用户态读取MsgHdr，以及写入MsgHdr
