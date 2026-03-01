use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::Arc,
};
use core::ptr;

use log::{info, warn};
use system_error::SystemError;
use virtio_drivers::transport::Transport;

use crate::{
    driver::base::device::{Device, DeviceId},
    libs::spinlock::SpinLock,
};

use super::transport::VirtIOTransport;

const VIRTIO_FS_TAG_LEN: usize = 36;
const VIRTIO_FS_REQUEST_QUEUE_BASE: u16 = 1;

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
struct VirtioFsInstanceState {
    transport: Option<VirtioFsTransportHolder>,
    session_active: bool,
}

#[derive(Debug)]
pub struct VirtioFsInstance {
    tag: String,
    num_request_queues: u32,
    dev_id: Arc<DeviceId>,
    state: SpinLock<VirtioFsInstanceState>,
}

impl VirtioFsInstance {
    fn new(
        tag: String,
        num_request_queues: u32,
        dev_id: Arc<DeviceId>,
        transport: VirtIOTransport,
    ) -> Self {
        Self {
            tag,
            num_request_queues,
            dev_id,
            state: SpinLock::new(VirtioFsInstanceState {
                transport: Some(VirtioFsTransportHolder(transport)),
                session_active: false,
            }),
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
        VIRTIO_FS_REQUEST_QUEUE_BASE
            .checked_add(slot as u16)
            .filter(|idx| {
                (*idx as usize)
                    < (VIRTIO_FS_REQUEST_QUEUE_BASE as usize + self.request_queue_count())
            })
    }

    pub fn take_transport_for_session(&self) -> Result<VirtIOTransport, SystemError> {
        let mut state = self.state.lock_irqsave();
        if state.session_active {
            return Err(SystemError::EBUSY);
        }

        let transport = state.transport.take().ok_or(SystemError::EBUSY)?.0;
        state.session_active = true;
        Ok(transport)
    }

    pub fn put_transport_after_session(&self, transport: VirtIOTransport) {
        let mut state = self.state.lock_irqsave();
        state.transport = Some(VirtioFsTransportHolder(transport));
        state.session_active = false;
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
    if nrqs == 0 {
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

    let instance = Arc::new(VirtioFsInstance::new(
        tag.clone(),
        nrqs,
        dev_id.clone(),
        transport,
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
