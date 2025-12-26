use crate::{
    driver::net::Iface,
    filesystem::vfs::{
        fasync::FAsyncItem, file::File, FilePrivateData, FileType, IndexNode, InodeMode, Metadata,
        PollableInode,
    },
    libs::spinlock::SpinLockGuard,
    net::posix::SockAddrIn,
    process::ProcessManager,
    syscall::user_access::{UserBufferReader, UserBufferWriter},
};
use alloc::{string::String, sync::Arc, vec::Vec};
use core::sync::atomic::Ordering;
use system_error::SystemError;

use super::Socket;

// Socket ioctl commands
const SIOCGIFCONF: u32 = 0x8912; // Get interface list

// Constants for network interface structures
const IFNAMSIZ: usize = 16;

/// ## ifreq - Interface request structure
/// Used for socket ioctls. Must match C struct layout.
/// On Linux x86_64: sizeof(ifreq) = 40 bytes
/// - ifr_name: 16 bytes (IFNAMSIZ)
/// - ifr_ifru: 24 bytes (union, largest member is struct ifmap)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct IfReq {
    ifr_name: [u8; IFNAMSIZ], // Interface name (16 bytes)
    ifr_ifru: [u8; 24],       // Union for various data (24 bytes to match Linux)
}

impl Default for IfReq {
    fn default() -> Self {
        Self {
            ifr_name: [0u8; IFNAMSIZ],
            ifr_ifru: [0u8; 24],
        }
    }
}

impl IfReq {
    /// Set the sockaddr_in in ifr_ifru
    fn set_sockaddr_in(&mut self, addr: &SockAddrIn) {
        // Copy sockaddr_in into the first 16 bytes of ifr_ifru
        let addr_bytes: &[u8] =
            unsafe { core::slice::from_raw_parts(addr as *const SockAddrIn as *const u8, 16) };
        self.ifr_ifru[..16].copy_from_slice(addr_bytes);
    }
}

/// ## ifconf - Interface configuration structure
/// Used by SIOCGIFCONF. Must match C struct layout.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct IfConf {
    ifc_len: i32,   // Size of buffer or number of bytes returned
    ifc_buf: usize, // Pointer to buffer (union with ifc_req)
}

/// Handle SIOCGIFCONF ioctl
/// This ioctl returns a list of interface (network layer) addresses.
fn handle_siocgifconf(data: usize) -> Result<usize, SystemError> {
    if data == 0 {
        return Err(SystemError::EFAULT);
    }

    // Read ifconf structure from user space
    let user_reader =
        UserBufferReader::new(data as *const IfConf, core::mem::size_of::<IfConf>(), true)?;
    let ifconf = user_reader.buffer_protected(0)?.read_one::<IfConf>(0)?;

    let ifc_len = ifconf.ifc_len;
    let ifc_buf = ifconf.ifc_buf;

    // Get current network namespace and enumerate interfaces
    let netns = ProcessManager::current_netns();
    let device_list = netns.device_list();

    // Calculate how many ifreq structures we can fit
    let ifreq_size = core::mem::size_of::<IfReq>();

    // Collect interface information
    let mut ifreqs: Vec<IfReq> = Vec::new();

    for (_, iface) in device_list.iter() {
        // Get interface name
        let iface_name = iface.iface_name();

        // Get IPv4 address if available
        if let Some(ipv4_addr) = iface.common().ipv4_addr() {
            let mut ifreq = IfReq::default();

            // Copy interface name
            let name_bytes = iface_name.as_bytes();
            let copy_len = core::cmp::min(name_bytes.len(), IFNAMSIZ - 1);
            ifreq.ifr_name[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

            // Set up sockaddr_in for IPv4
            let addr = SockAddrIn {
                sin_family: 2, // AF_INET
                sin_port: 0,
                sin_addr: u32::from_ne_bytes(ipv4_addr.octets()),
                sin_zero: [0u8; 8],
            };
            ifreq.set_sockaddr_in(&addr);

            ifreqs.push(ifreq);
        }
    }

    // Linux 语义：SIOCGIFCONF 至少应该能看到 lo（loopback）。
    // 在 DragonOS 的某些启动/配置场景下，loopback 设备可能尚未被注册到 netns，
    // 但用户态仍期望能通过该 ioctl 观察到 127.0.0.1。
    let lo_name = "lo";
    let already_added = ifreqs.iter().any(|req| {
        // ifr_name 是以 NUL 结尾的 C 字符串。
        let end = req
            .ifr_name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(IFNAMSIZ);
        &req.ifr_name[..end] == lo_name.as_bytes()
    });

    if !already_added {
        let ipv4_addr = netns
            .loopback_iface()
            .and_then(|lo| lo.as_ref().common().ipv4_addr())
            .unwrap_or(core::net::Ipv4Addr::new(127, 0, 0, 1));

        let mut ifreq = IfReq::default();
        ifreq.ifr_name[..2].copy_from_slice(lo_name.as_bytes());

        let addr = SockAddrIn {
            sin_family: 2, // AF_INET
            sin_port: 0,
            sin_addr: u32::from_ne_bytes(ipv4_addr.octets()),
            sin_zero: [0u8; 8],
        };
        ifreq.set_sockaddr_in(&addr);
        ifreqs.push(ifreq);
    }

    // Move "lo" to the front if it exists (some tests expect loopback first)
    if let Some(lo_idx) = ifreqs.iter().position(|req| {
        let name_str = core::str::from_utf8(&req.ifr_name)
            .unwrap_or("")
            .trim_end_matches('\0');
        name_str == "lo"
    }) {
        if lo_idx > 0 {
            let lo_ifreq = ifreqs.remove(lo_idx);
            ifreqs.insert(0, lo_ifreq);
        }
    }

    // If ifc_buf is NULL (0), just return the total size needed
    if ifc_buf == 0 {
        let total_size = ifreqs.len() * ifreq_size;
        // Write back the required size
        let mut user_writer =
            UserBufferWriter::new(data as *mut IfConf, core::mem::size_of::<IfConf>(), true)?;
        let result_ifconf = IfConf {
            ifc_len: total_size as i32,
            ifc_buf: 0,
        };
        user_writer
            .buffer_protected(0)?
            .write_one(0, &result_ifconf)?;
        return Ok(0);
    }

    // Calculate how many complete ifreq structures fit in the buffer
    let max_ifreqs = if ifc_len >= 0 {
        (ifc_len as usize) / ifreq_size
    } else {
        0
    };

    // Don't return partial ifreq structures
    let num_to_copy = core::cmp::min(ifreqs.len(), max_ifreqs);
    let bytes_to_copy = num_to_copy * ifreq_size;

    // Validate the user buffer pointer before writing
    // If the buffer pointer is invalid, return EFAULT
    if num_to_copy > 0 {
        // Try to create a user buffer writer to validate the pointer
        let user_buf_writer_result = UserBufferWriter::new(ifc_buf as *mut u8, bytes_to_copy, true);
        if user_buf_writer_result.is_err() {
            return Err(SystemError::EFAULT);
        }
        let mut user_buf_writer = user_buf_writer_result.unwrap();

        // Build a contiguous buffer with all ifreq structures
        let mut all_data: Vec<u8> = Vec::with_capacity(bytes_to_copy);
        for ifreq in ifreqs.iter().take(num_to_copy) {
            let ifreq_bytes: &[u8] = unsafe {
                core::slice::from_raw_parts(ifreq as *const IfReq as *const u8, ifreq_size)
            };
            all_data.extend_from_slice(ifreq_bytes);
        }

        // Write all data at once
        user_buf_writer
            .buffer_protected(0)?
            .write_to_user(0, &all_data)?;
    }

    // Write back the actual size returned
    let mut user_writer =
        UserBufferWriter::new(data as *mut IfConf, core::mem::size_of::<IfConf>(), true)?;
    let result_ifconf = IfConf {
        ifc_len: bytes_to_copy as i32,
        ifc_buf,
    };
    user_writer
        .buffer_protected(0)?
        .write_one(0, &result_ifconf)?;

    Ok(0)
}

impl<T: Socket + 'static> IndexNode for T {
    fn open(
        &self,
        data: SpinLockGuard<FilePrivateData>,
        _: &crate::filesystem::vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        match &*data {
            FilePrivateData::SocketCreate => {
                self.open_file_counter().fetch_add(1, Ordering::Release);
                Ok(())
            }
            _ => Err(SystemError::ENXIO),
        }
    }

    fn close(&self, _: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        // Only tear down the socket on the final close.
        if self.open_file_counter().fetch_sub(1, Ordering::AcqRel) == 1 {
            self.do_close()
        } else {
            Ok(())
        }
    }

    fn read_at(
        &self,
        _: usize,
        _: usize,
        buf: &mut [u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // Drop the lock guard before calling self.read() to avoid holding the lock
        // across a potentially blocking or reentrant operation. This prevents deadlocks
        // and preemption issues.
        drop(data);
        self.read(buf)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &[u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        drop(data);
        self.write(buf)
    }

    fn resize(&self, _len: usize) -> Result<(), SystemError> {
        Ok(())
    }

    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        unreachable!("Socket does not have a file system")
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        Err(SystemError::ENOTDIR)
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Ok(self)
    }

    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        let mut md = Metadata::new(FileType::Socket, InodeMode::from_bits_truncate(0o755));
        md.inode_id = self.socket_inode_id();
        md.mode |= InodeMode::S_IFSOCK;
        Ok(md)
    }

    // TODO: implement ioctl for socket
    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        match cmd {
            SIOCGIFCONF => handle_siocgifconf(data),
            _ => Socket::ioctl(self, cmd, data, private_data),
        }
    }

    fn as_socket(&self) -> Option<&dyn Socket> {
        Some(self)
    }
}

impl<T: Socket + 'static> PollableInode for T {
    fn poll(&self, _: &FilePrivateData) -> Result<usize, SystemError> {
        Ok(self.check_io_event().bits() as usize)
    }

    fn add_epitem(
        &self,
        epitem: Arc<crate::filesystem::epoll::EPollItem>,
        _: &FilePrivateData,
    ) -> Result<(), SystemError> {
        self.epoll_items().add(epitem);
        return Ok(());
    }

    fn remove_epitem(
        &self,
        epitm: &Arc<crate::filesystem::epoll::EPollItem>,
        _: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let _ = self.epoll_items().remove(&epitm.epoll());
        return Ok(());
    }

    fn add_fasync(
        &self,
        fasync_item: Arc<FAsyncItem>,
        _: &FilePrivateData,
    ) -> Result<(), SystemError> {
        self.fasync_items().add(fasync_item);
        Ok(())
    }

    fn remove_fasync(
        &self,
        file: &alloc::sync::Weak<File>,
        _: &FilePrivateData,
    ) -> Result<(), SystemError> {
        self.fasync_items().remove(file);
        Ok(())
    }
}
