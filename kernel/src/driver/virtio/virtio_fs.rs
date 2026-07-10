use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
};
use core::ptr;

use log::{info, warn};
use system_error::SystemError;
use virtio_drivers::transport::Transport;

use crate::{
    driver::base::device::{Device, DeviceId},
    exception::{irqdesc::IrqReturn, IrqNumber},
    filesystem::fuse::{conn::FuseConn, stats},
    libs::{spinlock::SpinLock, wait_queue::WaitQueue},
};

use super::{
    irq::{virtio_irq_manager, VirtioIrqCallback},
    transport::VirtIOTransport,
    transport_pci::PciInterruptAck,
};

const VIRTIO_FS_TAG_LEN: usize = 36;
const VIRTIO_FS_REQUEST_QUEUE_BASE: u16 = 1;
const VIRTIO_FS_MAX_REQUEST_QUEUES: u32 = 64;

#[repr(C, packed)]
struct VirtioFsConfig {
    tag: [u8; VIRTIO_FS_TAG_LEN],
    num_request_queues: u32,
}

struct VirtioFsTransportHolder(VirtIOTransport);

impl core::fmt::Debug for VirtioFsTransportHolder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("VirtioFsTransportHolder(..)")
    }
}

// Safety: virtio-fs transport is always moved by ownership under SpinLock
// protection, and never accessed concurrently through shared references.
unsafe impl Send for VirtioFsTransportHolder {}

#[derive(Debug)]
struct VirtioFsActiveBridge {
    session_id: u64,
    conn: Weak<FuseConn>,
}

#[derive(Debug)]
struct VirtioFsInstanceState {
    transport: Option<VirtioFsTransportHolder>,
    session_active: bool,
    active_session_id: u64,
    released_session_id: u64,
    next_session_id: u64,
    active_bridge: Option<VirtioFsActiveBridge>,
}

impl VirtioFsInstanceState {
    fn release_active_session_without_transport(&mut self, session_id: u64) -> bool {
        if !self.session_active || self.active_session_id != session_id {
            return false;
        }

        self.active_bridge = None;
        self.released_session_id = session_id;
        self.active_session_id = 0;
        self.session_active = false;
        true
    }

    fn is_session_released(&self, session_id: u64) -> bool {
        self.released_session_id == session_id
    }
}

#[derive(Debug)]
pub struct VirtioFsInstance {
    tag: String,
    num_request_queues: u32,
    dev_id: Arc<DeviceId>,
    irq_wake_enabled: bool,
    irq_is_msix: bool,
    irq_ack: Option<PciInterruptAck>,
    state: SpinLock<VirtioFsInstanceState>,
    session_wait: WaitQueue,
}

impl VirtioFsInstance {
    fn new(
        tag: String,
        num_request_queues: u32,
        dev_id: Arc<DeviceId>,
        transport: VirtIOTransport,
        irq_wake_enabled: bool,
        irq_is_msix: bool,
        irq_ack: Option<PciInterruptAck>,
    ) -> Self {
        Self {
            tag,
            num_request_queues,
            dev_id,
            irq_wake_enabled,
            irq_is_msix,
            irq_ack,
            state: SpinLock::new(VirtioFsInstanceState {
                transport: Some(VirtioFsTransportHolder(transport)),
                session_active: false,
                active_session_id: 0,
                released_session_id: 0,
                next_session_id: 1,
                active_bridge: None,
            }),
            session_wait: WaitQueue::default(),
        }
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }

    pub fn dev_id(&self) -> &Arc<DeviceId> {
        &self.dev_id
    }

    pub fn num_request_queues(&self) -> u32 {
        self.num_request_queues
    }

    pub fn hiprio_queue_index(&self) -> u16 {
        0
    }

    pub fn request_queue_count(&self) -> usize {
        self.num_request_queues as usize
    }

    pub fn request_queue_index_by_slot(&self, slot: usize) -> Option<u16> {
        if slot >= self.request_queue_count() {
            return None;
        }
        let slot = u16::try_from(slot).ok()?;
        VIRTIO_FS_REQUEST_QUEUE_BASE
            .checked_add(slot)
            .filter(|idx| {
                (*idx as usize)
                    < (VIRTIO_FS_REQUEST_QUEUE_BASE as usize + self.request_queue_count())
            })
    }

    pub fn take_transport_for_session(&self) -> Result<(VirtIOTransport, u64), SystemError> {
        let mut state = self.state.lock_irqsave();
        if state.session_active {
            return Err(SystemError::EBUSY);
        }

        let transport = state.transport.take().ok_or(SystemError::EBUSY)?.0;
        let session_id = state.next_session_id;
        state.next_session_id = state.next_session_id.wrapping_add(1);
        state.active_session_id = session_id;
        state.session_active = true;
        state.active_bridge = None;
        Ok((transport, session_id))
    }

    pub fn install_bridge_wake(&self, session_id: u64, conn: &Arc<FuseConn>) {
        let mut state = self.state.lock_irqsave();
        if state.session_active && state.active_session_id == session_id {
            conn.install_bridge_wake();
            state.active_bridge = Some(VirtioFsActiveBridge {
                session_id,
                conn: Arc::downgrade(conn),
            });
        }
    }

    pub fn enable_irq_wake(self: &Arc<Self>) -> bool {
        if !self.irq_wake_enabled {
            return false;
        }
        match virtio_irq_manager().register_callback(self.dev_id.clone(), self.clone()) {
            Ok(()) => true,
            Err(e) => {
                warn!(
                    "virtio-fs: failed to register irq callback for tag='{}' dev={:?}: {:?}; use polling fallback",
                    self.tag, self.dev_id, e
                );
                false
            }
        }
    }

    pub fn disable_irq_wake(&self) {
        if self.irq_wake_enabled {
            virtio_irq_manager().unregister_callback(&self.dev_id);
        }
    }

    pub fn clear_bridge_wake(&self, session_id: u64) {
        let conn = {
            let mut state = self.state.lock_irqsave();
            if state
                .active_bridge
                .as_ref()
                .is_some_and(|bridge| bridge.session_id == session_id)
            {
                state
                    .active_bridge
                    .take()
                    .and_then(|bridge| bridge.conn.upgrade())
            } else {
                None
            }
        };
        if let Some(conn) = conn {
            conn.clear_bridge_wake();
        }
    }

    pub fn put_transport_after_session(&self, transport: VirtIOTransport) {
        let conn = {
            let mut state = self.state.lock_irqsave();
            let conn = state
                .active_bridge
                .take()
                .and_then(|bridge| bridge.conn.upgrade());
            state.transport = Some(VirtioFsTransportHolder(transport));
            state.released_session_id = state.active_session_id;
            state.active_session_id = 0;
            state.session_active = false;
            conn
        };
        if let Some(conn) = conn {
            conn.clear_bridge_wake();
        }
        self.session_wait.wakeup(None);
    }

    /// Publish logical session completion while keeping an unsafe transport quarantined.
    ///
    /// This is used only when device reset did not complete. The caller must retain the
    /// transport, queues, and DMA buffers because the device may still access them.
    pub fn release_session_without_transport(&self, session_id: u64) -> bool {
        let conn = {
            let mut state = self.state.lock_irqsave();
            if !state.session_active || state.active_session_id != session_id {
                return false;
            }
            let conn = state
                .active_bridge
                .as_ref()
                .and_then(|bridge| bridge.conn.upgrade());
            if !state.release_active_session_without_transport(session_id) {
                return false;
            }
            conn
        };
        if let Some(conn) = conn {
            conn.clear_bridge_wake();
        }
        self.session_wait.wakeup(None);
        true
    }

    pub fn wait_session_released(&self, session_id: u64) {
        self.session_wait.wait_until(|| {
            let state = self.state.lock_irqsave();
            if state.is_session_released(session_id) {
                Some(())
            } else {
                None
            }
        });
    }

    fn wake_bridge_from_irq(&self) -> IrqReturn {
        let mut owned_irq = self.irq_is_msix;
        if let Some(irq_ack) = self.irq_ack.as_ref() {
            let acked = irq_ack.ack_interrupt();
            if !acked && !self.irq_is_msix {
                return IrqReturn::NotHandled;
            }
            owned_irq = acked || self.irq_is_msix;
        }

        let (session_id, conn) = {
            let state = self.state.lock_irqsave();
            let Some(bridge) = state.active_bridge.as_ref() else {
                stats::on_virtiofs_irq_no_active_conn();
                return if owned_irq {
                    IrqReturn::Handled
                } else {
                    IrqReturn::NotHandled
                };
            };
            if !state.session_active || bridge.session_id != state.active_session_id {
                stats::on_virtiofs_irq_stale_session();
                return if owned_irq {
                    IrqReturn::Handled
                } else {
                    IrqReturn::NotHandled
                };
            }
            (bridge.session_id, bridge.conn.clone())
        };

        let Some(conn) = conn.upgrade() else {
            stats::on_virtiofs_irq_weak_upgrade_failed();
            self.clear_bridge_wake(session_id);
            return if owned_irq {
                IrqReturn::Handled
            } else {
                IrqReturn::NotHandled
            };
        };

        conn.wake_bridge_irq_safe(stats::VirtioFsBridgeWakeSource::Completion);
        IrqReturn::Handled
    }
}

impl VirtioIrqCallback for VirtioFsInstance {
    fn handle_irq(&self, _irq: IrqNumber) -> Result<IrqReturn, SystemError> {
        Ok(self.wake_bridge_from_irq())
    }
}

lazy_static! {
    static ref VIRTIO_FS_INSTANCES: SpinLock<BTreeMap<String, Arc<VirtioFsInstance>>> =
        SpinLock::new(BTreeMap::new());
}

fn read_config(transport: &VirtIOTransport) -> Result<(String, u32), SystemError> {
    let cfg = transport
        .config_space::<VirtioFsConfig>()
        .map_err(|_| SystemError::EINVAL)?;

    let base = cfg.as_ptr() as *const u8;
    let mut tag_raw = [0u8; VIRTIO_FS_TAG_LEN];
    for (i, b) in tag_raw.iter_mut().enumerate() {
        *b = unsafe { ptr::read_volatile(base.add(i)) };
    }

    let tag_len = tag_raw
        .iter()
        .position(|x| *x == 0)
        .unwrap_or(VIRTIO_FS_TAG_LEN);
    if tag_len == 0 {
        return Err(SystemError::EINVAL);
    }

    let tag = core::str::from_utf8(&tag_raw[..tag_len])
        .map_err(|_| SystemError::EINVAL)?
        .to_string();
    let mut nrqs_raw = [0u8; core::mem::size_of::<u32>()];
    for (i, b) in nrqs_raw.iter_mut().enumerate() {
        *b = unsafe { ptr::read_volatile(base.add(VIRTIO_FS_TAG_LEN + i)) };
    }
    let nrqs = u32::from_le_bytes(nrqs_raw);
    if nrqs == 0 || nrqs > VIRTIO_FS_MAX_REQUEST_QUEUES {
        return Err(SystemError::EINVAL);
    }

    Ok((tag, nrqs))
}

pub fn virtio_fs_find_instance(tag: &str) -> Option<Arc<VirtioFsInstance>> {
    VIRTIO_FS_INSTANCES.lock_irqsave().get(tag).cloned()
}

pub fn virtio_fs(
    transport: VirtIOTransport,
    dev_id: Arc<DeviceId>,
    _dev_parent: Option<Arc<dyn Device>>,
) {
    let (tag, nrqs) = match read_config(&transport) {
        Ok(v) => v,
        Err(e) => {
            warn!(
                "virtio-fs: failed to read config for device {:?}: {:?}",
                dev_id, e
            );
            return;
        }
    };

    {
        let map = VIRTIO_FS_INSTANCES.lock_irqsave();
        if map.contains_key(&tag) {
            warn!(
                "virtio-fs: duplicated tag '{}' for device {:?}, ignore new device",
                tag, dev_id
            );
            return;
        }
    }

    let mut irq_wake_enabled = false;
    let mut irq_is_msix = false;
    let mut irq_ack = None;
    if matches!(transport, VirtIOTransport::Pci(_)) {
        if let Err(e) = transport.setup_irq(dev_id.clone()) {
            warn!(
                "virtio-fs: failed to setup irq for tag='{}' dev={:?}: {:?}; use polling fallback",
                tag, dev_id, e
            );
        } else {
            irq_wake_enabled = true;
            irq_is_msix = transport.irq_is_msix();
            if let VirtIOTransport::Pci(pci_transport) = &transport {
                irq_ack = Some(pci_transport.interrupt_ack());
            }
        }
    } else {
        warn!(
            "virtio-fs: tag='{}' dev={:?} has no event-driven IRQ wake path; use polling fallback",
            tag, dev_id
        );
    }

    let instance = Arc::new(VirtioFsInstance::new(
        tag.clone(),
        nrqs,
        dev_id.clone(),
        transport,
        irq_wake_enabled,
        irq_is_msix,
        irq_ack,
    ));

    let mut map = VIRTIO_FS_INSTANCES.lock_irqsave();
    if map.contains_key(&tag) {
        warn!(
            "virtio-fs: duplicated tag '{}' for device {:?}, ignore new device",
            tag, dev_id
        );
        return;
    }

    map.insert(tag.clone(), instance);
    info!(
        "virtio-fs: registered instance tag='{}' dev={:?} request_queues={}",
        tag, dev_id, nrqs
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_state(session_id: u64) -> VirtioFsInstanceState {
        VirtioFsInstanceState {
            transport: None,
            session_active: true,
            active_session_id: session_id,
            released_session_id: 0,
            next_session_id: session_id + 1,
            active_bridge: None,
        }
    }

    #[test]
    fn reset_timeout_releases_session_without_transport() {
        let mut state = active_state(7);

        assert!(state.release_active_session_without_transport(7));
        assert!(state.is_session_released(7));
        assert!(!state.session_active);
        assert_eq!(state.active_session_id, 0);
        assert!(state.transport.is_none());
    }

    #[test]
    fn stale_session_cannot_release_active_session() {
        let mut state = active_state(9);

        assert!(!state.release_active_session_without_transport(8));
        assert!(state.session_active);
        assert_eq!(state.active_session_id, 9);
        assert!(!state.is_session_released(8));
    }
}
