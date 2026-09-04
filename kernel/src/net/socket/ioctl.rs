//! Linux-compatible network-device ioctls shared by all socket families.
//!
//! Query commands read the namespace captured by the socket at creation time,
//! matching Linux `sock_net(sk)`. Query commands return one device property
//! through the `ifreq` union. Mutation commands only translate the ABI and
//! delegate to the same RTNL-serialized link core as rtnetlink.

use alloc::{sync::Arc, vec::Vec};
use core::mem::size_of;

use system_error::SystemError;

use crate::{
    driver::net::{types::InterfaceFlags, Iface},
    net::{
        link::{LinkFlagsUpdate, LinkMtuUpdate, LinkTarget, LinkUpdate},
        posix::SockAddrIn,
        socket::IFNAMSIZ,
    },
    process::{
        cred::{ns_capable, CAPFlags},
        namespace::net_namespace::NetNamespace,
    },
    syscall::user_access::{UserBufferReader, UserBufferWriter},
};

pub(super) const SIOCGIFCONF: u32 = 0x8912;
pub(super) const SIOCGIFFLAGS: u32 = 0x8913;
pub(super) const SIOCSIFFLAGS: u32 = 0x8914;
pub(super) const SIOCGIFMTU: u32 = 0x8921;
pub(super) const SIOCSIFMTU: u32 = 0x8922;
pub(super) const SIOCGIFHWADDR: u32 = 0x8927;
pub(super) const SIOCGIFINDEX: u32 = 0x8933;

/// Native x86_64 Linux `struct ifreq` layout.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct IfReq {
    ifr_name: [u8; IFNAMSIZ],
    ifr_ifru: [u8; 24],
}

const _: () = assert!(size_of::<IfReq>() == 40);

impl Default for IfReq {
    fn default() -> Self {
        Self {
            ifr_name: [0; IFNAMSIZ],
            ifr_ifru: [0; 24],
        }
    }
}

impl IfReq {
    fn canonical_name_bytes(&mut self) -> &[u8] {
        self.ifr_name[IFNAMSIZ - 1] = 0;
        let nul = self
            .ifr_name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(IFNAMSIZ);
        let name = &self.ifr_name[..nul];
        let alias = name.iter().position(|byte| *byte == b':').unwrap_or(nul);
        &name[..alias]
    }

    fn lookup_name(&mut self) -> Result<&str, SystemError> {
        let name =
            core::str::from_utf8(self.canonical_name_bytes()).map_err(|_| SystemError::ENODEV)?;
        if name.is_empty() {
            return Err(SystemError::ENODEV);
        }
        Ok(name)
    }

    fn set_i32(&mut self, value: i32) {
        self.ifr_ifru[..size_of::<i32>()].copy_from_slice(&value.to_ne_bytes());
    }

    fn get_i32(&self) -> i32 {
        i32::from_ne_bytes(
            self.ifr_ifru[..size_of::<i32>()]
                .try_into()
                .expect("ifreq union contains an i32"),
        )
    }

    fn get_flags(&self) -> InterfaceFlags {
        let value = i16::from_ne_bytes(
            self.ifr_ifru[..size_of::<i16>()]
                .try_into()
                .expect("ifreq union contains an i16"),
        );
        InterfaceFlags::from_bits_truncate((value as u16) as u32)
    }

    fn set_flags(&mut self, flags: u32) {
        let flags = flags as i16;
        self.ifr_ifru[..size_of::<i16>()].copy_from_slice(&flags.to_ne_bytes());
    }

    fn set_hwaddr(&mut self, iface: &Arc<dyn Iface>) {
        let family = iface.type_() as u16;
        self.ifr_ifru[..size_of::<u16>()].copy_from_slice(&family.to_ne_bytes());
        self.ifr_ifru[size_of::<u16>()..size_of::<u16>() + 6]
            .copy_from_slice(iface.mac().as_bytes());
    }

    fn set_name(&mut self, name: &str) {
        let bytes = name.as_bytes();
        let len = bytes.len().min(IFNAMSIZ - 1);
        self.ifr_name[..len].copy_from_slice(&bytes[..len]);
    }

    fn set_sockaddr_in(&mut self, addr: &SockAddrIn) {
        let bytes = unsafe {
            core::slice::from_raw_parts(
                addr as *const SockAddrIn as *const u8,
                size_of::<SockAddrIn>(),
            )
        };
        self.ifr_ifru[..size_of::<SockAddrIn>()].copy_from_slice(bytes);
    }
}

fn read_ifreq(data: usize) -> Result<IfReq, SystemError> {
    let reader = UserBufferReader::new(data as *const IfReq, size_of::<IfReq>(), true)?;
    reader.buffer_protected(0)?.read_one(0)
}

fn write_ifreq(data: usize, ifreq: &IfReq) -> Result<(), SystemError> {
    let mut writer = UserBufferWriter::new(data as *mut IfReq, size_of::<IfReq>(), true)?;
    writer.buffer_protected(0)?.write_one(0, ifreq)
}

fn find_iface(netns: &Arc<NetNamespace>, name: &str) -> Result<Arc<dyn Iface>, SystemError> {
    netns
        .device_list()
        .iter()
        .find_map(|(_, iface)| (iface.iface_name() == name).then(|| iface.clone()))
        .ok_or(SystemError::ENODEV)
}

pub(super) fn handle_netdev_query(
    netns: Arc<NetNamespace>,
    cmd: u32,
    data: usize,
) -> Result<usize, SystemError> {
    let mut ifreq = read_ifreq(data)?;
    let iface = find_iface(&netns, ifreq.lookup_name()?)?;

    match cmd {
        SIOCGIFINDEX => ifreq.set_i32(iface.nic_id() as i32),
        SIOCGIFFLAGS => ifreq.set_flags(iface.user_visible_flags().bits()),
        SIOCGIFMTU => ifreq.set_i32(iface.mtu() as i32),
        SIOCGIFHWADDR => ifreq.set_hwaddr(&iface),
        _ => return Err(SystemError::ENOTTY),
    }

    write_ifreq(data, &ifreq)?;
    Ok(0)
}

pub(super) fn handle_netdev_mutation(
    netns: Arc<NetNamespace>,
    cmd: u32,
    data: usize,
) -> Result<usize, SystemError> {
    // Linux copies the whole ifreq before the capability check, so EFAULT has
    // precedence over EPERM and device lookup errors.
    let mut ifreq = read_ifreq(data)?;
    let mut name_storage = [0u8; IFNAMSIZ];
    let name_len = {
        let name = ifreq.canonical_name_bytes();
        name_storage[..name.len()].copy_from_slice(name);
        name.len()
    };
    if !ns_capable(netns.user_ns(), CAPFlags::CAP_NET_ADMIN) {
        return Err(SystemError::EPERM);
    }
    let name = core::str::from_utf8(&name_storage[..name_len]).map_err(|_| SystemError::ENODEV)?;
    if name.is_empty() {
        return Err(SystemError::ENODEV);
    }

    let update = match cmd {
        SIOCSIFFLAGS => LinkUpdate {
            flags: Some(LinkFlagsUpdate::Replace(ifreq.get_flags())),
            ..Default::default()
        },
        SIOCSIFMTU => LinkUpdate {
            mtu: Some(LinkMtuUpdate::Ioctl(ifreq.get_i32())),
            ..Default::default()
        },
        _ => return Err(SystemError::ENOTTY),
    };

    let rtnl = crate::net::rtnl::lock();
    let committed = crate::net::link::mutate_link(&rtnl, &netns, LinkTarget::Name(name), update)?;
    super::netlink::notify_link_commit(&netns, committed);
    Ok(0)
}

/// Native x86_64 Linux `struct ifconf` layout.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct IfConf {
    ifc_len: i32,
    ifc_buf: usize,
}

const _: () = assert!(size_of::<IfConf>() == 16);

fn read_ifconf(data: usize) -> Result<IfConf, SystemError> {
    let reader = UserBufferReader::new(data as *const IfConf, size_of::<IfConf>(), true)?;
    reader.buffer_protected(0)?.read_one(0)
}

fn write_ifconf_len(data: usize, len: i32) -> Result<(), SystemError> {
    let mut writer = UserBufferWriter::new(data as *mut i32, size_of::<i32>(), true)?;
    writer.buffer_protected(0)?.write_one(0, &len)
}

fn snapshot_ifconf(netns: &Arc<NetNamespace>) -> Result<Vec<IfReq>, SystemError> {
    let _rtnl_guard = crate::net::rtnl::lock();
    let devices = netns.device_list();
    let mut snapshot = Vec::new();
    snapshot
        .try_reserve_exact(devices.len())
        .map_err(|_| SystemError::ENOMEM)?;
    for (_, iface) in devices.iter() {
        let Some(ipv4) = iface.common().ipv4_addr() else {
            continue;
        };
        let mut ifreq = IfReq::default();
        ifreq.set_name(&iface.iface_name());
        ifreq.set_sockaddr_in(&SockAddrIn {
            sin_family: 2,
            sin_port: 0,
            sin_addr: u32::from_ne_bytes(ipv4.octets()),
            sin_zero: [0; 8],
        });
        snapshot.push(ifreq);
    }
    Ok(snapshot)
}

pub(super) fn handle_siocgifconf(
    netns: Arc<NetNamespace>,
    data: usize,
) -> Result<usize, SystemError> {
    let input = read_ifconf(data)?;
    let ifreqs = snapshot_ifconf(&netns)?;
    let required = ifreqs.len() * size_of::<IfReq>();

    if input.ifc_buf == 0 {
        write_ifconf_len(data, required as i32)?;
        return Ok(0);
    }

    let capacity = if input.ifc_len < 0 {
        0
    } else {
        input.ifc_len as usize / size_of::<IfReq>()
    };
    let count = capacity.min(ifreqs.len());
    let bytes = count * size_of::<IfReq>();
    if bytes != 0 {
        let mut writer = UserBufferWriter::new(input.ifc_buf as *mut u8, bytes, true)?;
        let mut buffer = writer.buffer_protected(0)?;
        for (index, ifreq) in ifreqs.iter().take(count).enumerate() {
            buffer.write_one(index * size_of::<IfReq>(), ifreq)?;
        }
    }

    write_ifconf_len(data, bytes as i32)?;
    Ok(0)
}
