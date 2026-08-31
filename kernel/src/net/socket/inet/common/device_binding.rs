use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use system_error::SystemError;

use crate::driver::net::Iface;
use crate::libs::mutex::{Mutex, MutexGuard};
use crate::net::socket::IFNAMSIZ;
use crate::process::cred::{ns_capable, CAPFlags};
use crate::process::namespace::net_namespace::NetNamespace;

/// Linux `sk_bound_dev_if` semantics shared by inet sockets.
///
/// The interface index is the sole authoritative state. Names are resolved in
/// the socket-owned network namespace when setting or getting the option, so
/// rename and device removal remain observable without cached device objects.
#[derive(Debug)]
pub struct SocketDeviceBinding {
    ifindex: AtomicUsize,
    update_lock: Mutex<()>,
}

impl Default for SocketDeviceBinding {
    fn default() -> Self {
        Self {
            ifindex: AtomicUsize::new(0),
            update_lock: Mutex::new(()),
        }
    }
}

impl SocketDeviceBinding {
    #[inline]
    pub fn ifindex(&self) -> usize {
        self.ifindex.load(Ordering::Acquire)
    }

    #[inline]
    pub fn allows(&self, ingress_ifindex: usize) -> bool {
        let bound = self.ifindex();
        bound == 0 || bound == ingress_ifindex
    }

    pub fn resolve_iface(
        &self,
        netns: &Arc<NetNamespace>,
    ) -> Result<Option<Arc<dyn Iface>>, SystemError> {
        let ifindex = self.ifindex();
        if ifindex == 0 {
            return Ok(None);
        }
        netns
            .device_list()
            .get(&ifindex)
            .cloned()
            .map(Some)
            .ok_or(SystemError::ENODEV)
    }

    /// Parse and authorize a `SO_BINDTODEVICE` update while serializing writers.
    /// Dropping the returned update without committing leaves the binding intact.
    pub fn prepare_update<'a>(
        &'a self,
        netns: &Arc<NetNamespace>,
        value: &[u8],
    ) -> Result<DeviceBindingUpdate<'a>, SystemError> {
        let guard = self.update_lock.lock();
        let end = value
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(value.len());
        let name = &value[..end];

        let (target_ifindex, target_iface) = if name.is_empty() {
            (0, None)
        } else {
            let iface = netns
                .device_list()
                .values()
                .find(|iface| iface.iface_name().as_bytes() == name)
                .cloned()
                .ok_or(SystemError::ENODEV)?;
            (iface.nic_id(), Some(iface))
        };

        // Linux resolves the requested name before checking the capability and
        // only requires CAP_NET_RAW when replacing an existing non-zero binding.
        if self.ifindex.load(Ordering::Relaxed) != 0
            && !ns_capable(netns.user_ns(), CAPFlags::CAP_NET_RAW)
        {
            return Err(SystemError::EPERM);
        }

        Ok(DeviceBindingUpdate {
            binding: self,
            _guard: guard,
            target_ifindex,
            target_iface,
        })
    }

    pub fn get(&self, netns: &Arc<NetNamespace>, value: &mut [u8]) -> Result<usize, SystemError> {
        let ifindex = self.ifindex();
        if ifindex == 0 {
            return Ok(0);
        }
        if value.len() < IFNAMSIZ {
            return Err(SystemError::EINVAL);
        }

        let iface = netns
            .device_list()
            .get(&ifindex)
            .cloned()
            .ok_or(SystemError::ENODEV)?;
        let name = iface.iface_name();
        let bytes = name.as_bytes();
        let copy_len = bytes.len().min(IFNAMSIZ - 1);
        value[..copy_len].copy_from_slice(&bytes[..copy_len]);
        value[copy_len] = 0;
        Ok(copy_len + 1)
    }
}

pub struct DeviceBindingUpdate<'a> {
    binding: &'a SocketDeviceBinding,
    _guard: MutexGuard<'a, ()>,
    target_ifindex: usize,
    target_iface: Option<Arc<dyn Iface>>,
}

impl DeviceBindingUpdate<'_> {
    #[inline]
    pub fn target_iface(&self) -> Option<Arc<dyn Iface>> {
        self.target_iface.clone()
    }

    #[inline]
    pub fn commit(&mut self) {
        self.binding
            .ifindex
            .store(self.target_ifindex, Ordering::Release);
    }
}
