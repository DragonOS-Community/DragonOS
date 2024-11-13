//
// posix.rs 记录了系统调用时用到的结构
//

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

        const NONBLOCK  = crate::filesystem::vfs::file::FileMode::O_NONBLOCK.bits();
        const CLOEXEC   = crate::filesystem::vfs::file::FileMode::O_CLOEXEC.bits();
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

use alloc::string::String;
use alloc::sync::Arc;
use core::ffi::CStr;
use unix::ns::abs::{alloc_abs_addr, look_up_abs_addr};

use crate::{
    filesystem::vfs::{FileType, IndexNode, ROOT_INODE, VFS_MAX_FOLLOW_SYMLINK_TIMES},
    mm::{verify_area, VirtAddr},
    net::socket::*,
    process::ProcessManager,
};
use smoltcp;
use system_error::SystemError::{self};

// 参考资料： https://pubs.opengroup.org/onlinepubs/9699919799/basedefs/netinet_in.h.html#tag_13_32
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockAddrIn {
    pub sin_family: u16,
    pub sin_port: u16,
    pub sin_addr: u32,
    pub sin_zero: [u8; 8],
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

impl SockAddr {
    /// @brief 把用户传入的SockAddr转换为Endpoint结构体
    pub fn to_endpoint(addr: *const SockAddr, len: u32) -> Result<Endpoint, SystemError> {
        use crate::net::socket::AddressFamily;

        let addr = unsafe { addr.as_ref() }.ok_or(SystemError::EFAULT)?;
        unsafe {
            match AddressFamily::try_from(addr.family)? {
                AddressFamily::INet => {
                    if len < addr.len()? {
                        log::error!("len < addr.len()");
                        return Err(SystemError::EINVAL);
                    }

                    let addr_in: SockAddrIn = addr.addr_in;

                    use smoltcp::wire;
                    let ip: wire::IpAddress = wire::IpAddress::from(wire::Ipv4Address::from_bits(
                        u32::from_be(addr_in.sin_addr),
                    ));
                    let port = u16::from_be(addr_in.sin_port);

                    return Ok(Endpoint::Ip(wire::IpEndpoint::new(ip, port)));
                }
                // AddressFamily::INet6 => {
                //     if len < addr.len()? {
                //         log::error!("len < addr.len()");
                //         return Err(SystemError::EINVAL);
                //     }
                //     log::debug!("INet6");
                //     let addr_in: SockAddrIn = addr.addr_in;

                //     use smoltcp::wire;
                //     let ip: wire::IpAddress = wire::IpAddress::from(wire::Ipv6Address::from_bits(
                //         u128::from_be(addr_in.sin_addr),
                //     ));
                //     let port = u16::from_be(addr_in.sin_port);

                //     return Ok(Endpoint::Ip(wire::IpEndpoint::new(ip, port)));
                // }
                AddressFamily::Unix => {
                    let addr_un: SockAddrUn = addr.addr_un;

                    if addr_un.sun_path[0] == 0 {
                        // 抽象地址空间，与文件系统没有关系
                        let path = CStr::from_bytes_until_nul(&addr_un.sun_path[1..])
                            .map_err(|_| {
                                log::error!("CStr::from_bytes_until_nul fail");
                                SystemError::EINVAL
                            })?
                            .to_str()
                            .map_err(|_| {
                                log::error!("CStr::to_str fail");
                                SystemError::EINVAL
                            })?;

                        // 向抽象地址管理器申请或查找抽象地址
                        let spath = String::from(path);
                        log::debug!("abs path: {}", spath);
                        let abs_find = match look_up_abs_addr(&spath) {
                            Ok(result) => result,
                            Err(_) => {
                                //未找到尝试分配abs
                                match alloc_abs_addr(spath.clone()) {
                                    Ok(result) => {
                                        log::debug!("alloc abs addr success!");
                                        return Ok(result);
                                    }
                                    Err(e) => {
                                        log::debug!("alloc abs addr failed!");
                                        return Err(e);
                                    }
                                };
                            }
                        };
                        log::debug!("find alloc abs addr success!");
                        return Ok(abs_find);
                    }

                    let path = CStr::from_bytes_until_nul(&addr_un.sun_path)
                        .map_err(|_| {
                            log::error!("CStr::from_bytes_until_nul fail");
                            SystemError::EINVAL
                        })?
                        .to_str()
                        .map_err(|_| {
                            log::error!("CStr::to_str fail");
                            SystemError::EINVAL
                        })?;

                    let (inode_begin, path) = crate::filesystem::vfs::utils::user_path_at(
                        &ProcessManager::current_pcb(),
                        crate::filesystem::vfs::fcntl::AtFlags::AT_FDCWD.bits(),
                        path.trim(),
                    )?;
                    let inode0: Result<Arc<dyn IndexNode>, SystemError> =
                        inode_begin.lookup_follow_symlink(&path, VFS_MAX_FOLLOW_SYMLINK_TIMES);

                    let inode = match inode0 {
                        Ok(inode) => inode,
                        Err(_) => {
                            let (filename, parent_path) =
                                crate::filesystem::vfs::utils::rsplit_path(&path);
                            // 查找父目录
                            log::debug!("filename {:?} parent_path {:?}", filename, parent_path);

                            let parent_inode: Arc<dyn IndexNode> =
                                ROOT_INODE().lookup(parent_path.unwrap_or("/"))?;
                            // 创建文件
                            let inode: Arc<dyn IndexNode> = match parent_inode.create(
                                filename,
                                FileType::File,
                                crate::filesystem::vfs::syscall::ModeType::from_bits_truncate(
                                    0o755,
                                ),
                            ) {
                                Ok(inode) => inode,
                                Err(e) => {
                                    log::debug!("inode create fail {:?}", e);
                                    return Err(e);
                                }
                            };
                            inode
                        }
                    };

                    return Ok(Endpoint::Unixpath((inode.metadata()?.inode_id, path)));
                }
                _ => {
                    log::warn!("not support address family {:?}", addr.family);
                    return Err(SystemError::EINVAL);
                }
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

    /// @brief 把SockAddr的数据写入用户空间
    ///
    /// @param addr 用户空间的SockAddr的地址
    /// @param len 要写入的长度
    ///
    /// @return 成功返回写入的长度，失败返回错误码
    pub unsafe fn write_to_user(
        &self,
        addr: *mut SockAddr,
        addr_len: *mut u32,
    ) -> Result<u32, SystemError> {
        // 当用户传入的地址或者长度为空时，直接返回0
        if addr.is_null() || addr_len.is_null() {
            return Ok(0);
        }

        // 检查用户传入的地址是否合法
        verify_area(
            VirtAddr::new(addr as usize),
            core::mem::size_of::<SockAddr>(),
        )
        .map_err(|_| SystemError::EFAULT)?;

        verify_area(
            VirtAddr::new(addr_len as usize),
            core::mem::size_of::<u32>(),
        )
        .map_err(|_| SystemError::EFAULT)?;

        let to_write = core::cmp::min(self.len()?, *addr_len);
        if to_write > 0 {
            let buf = core::slice::from_raw_parts_mut(addr as *mut u8, to_write as usize);
            buf.copy_from_slice(core::slice::from_raw_parts(
                self as *const SockAddr as *const u8,
                to_write as usize,
            ));
        }
        *addr_len = self.len()?;
        return Ok(to_write);
    }

    pub unsafe fn is_empty(&self) -> bool {
        unsafe { self.family == 0 && self.addr_ph.data == [0; 14] }
    }
}

impl From<Endpoint> for SockAddr {
    fn from(value: Endpoint) -> Self {
        match value {
            Endpoint::Ip(ip_endpoint) => match ip_endpoint.addr {
                smoltcp::wire::IpAddress::Ipv4(ipv4_addr) => {
                    let addr_in = SockAddrIn {
                        sin_family: AddressFamily::INet as u16,
                        sin_port: ip_endpoint.port.to_be(),
                        sin_addr: ipv4_addr.to_bits(),
                        sin_zero: [0; 8],
                    };

                    return SockAddr { addr_in };
                }
                _ => {
                    unimplemented!("not support ipv6");
                }
            },

            Endpoint::LinkLayer(link_endpoint) => {
                let addr_ll = SockAddrLl {
                    sll_family: AddressFamily::Packet as u16,
                    sll_protocol: 0,
                    sll_ifindex: link_endpoint.interface as u32,
                    sll_hatype: 0,
                    sll_pkttype: 0,
                    sll_halen: 0,
                    sll_addr: [0; 8],
                };

                return SockAddr { addr_ll };
            }

            Endpoint::Inode((_, path)) => {
                log::debug!("from unix path {:?}", path);
                let bytes = path.as_bytes();
                let mut sun_path = [0u8; 108];
                if bytes.len() <= 108 {
                    sun_path[..bytes.len()].copy_from_slice(bytes);
                } else {
                    panic!("unix address path too long!");
                }
                let addr_un = SockAddrUn {
                    sun_family: AddressFamily::Unix as u16,
                    sun_path,
                };
                return SockAddr { addr_un };
            }

            _ => {
                // todo: support other endpoint, like Netlink...
                unimplemented!("not support {value:?}");
            }
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MsgHdr {
    /// 指向一个SockAddr结构体的指针
    pub msg_name: *mut SockAddr,
    /// SockAddr结构体的大小
    pub msg_namelen: u32,
    /// scatter/gather array
    pub msg_iov: *mut crate::filesystem::vfs::syscall::IoVec,
    /// elements in msg_iov
    pub msg_iovlen: usize,
    /// 辅助数据
    pub msg_control: *mut u8,
    /// 辅助数据长度
    pub msg_controllen: u32,
    /// 接收到的消息的标志
    pub msg_flags: u32,
}

// TODO: 从用户态读取MsgHdr，以及写入MsgHdr
