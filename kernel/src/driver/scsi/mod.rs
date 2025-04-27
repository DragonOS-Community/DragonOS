use bitmap::traits::BitMapOps;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    init::initcall::INITCALL_POSTCORE,
    libs::spinlock::{SpinLock, SpinLockGuard},
};

use super::base::device::DevName;

static mut SCSI_MANAGER: Option<ScsiManager> = None;

#[inline]
pub fn scsi_manager() -> &'static ScsiManager {
    unsafe { SCSI_MANAGER.as_ref().unwrap() }
}

#[unified_init(INITCALL_POSTCORE)]
fn scsi_manager_init() -> Result<(), SystemError> {
    unsafe {
        SCSI_MANAGER = Some(ScsiManager::new());
    }
    Ok(())
}

pub struct ScsiManager {
    inner: SpinLock<InnerScsiManager>,
}

struct InnerScsiManager {
    id_bmp: bitmap::StaticBitmap<{ ScsiManager::MAX_DEVICES }>,
    devname: [Option<DevName>; ScsiManager::MAX_DEVICES],
}

impl ScsiManager {
    pub const MAX_DEVICES: usize = 25;

    pub fn new() -> Self {
        Self {
            inner: SpinLock::new(InnerScsiManager {
                id_bmp: bitmap::StaticBitmap::new(),
                devname: [const { None }; Self::MAX_DEVICES],
            }),
        }
    }

    fn inner(&self) -> SpinLockGuard<InnerScsiManager> {
        self.inner.lock()
    }

    pub fn alloc_id(&self) -> Option<DevName> {
        let mut inner = self.inner();
        let idx = inner.id_bmp.first_false_index()?;
        inner.id_bmp.set(idx, true);
        let name = Self::format_name(idx);
        inner.devname[idx] = Some(name.clone());
        Some(name)
    }

    /// Generate a new block device name like 'sda', 'sdb', etc.
    fn format_name(id: usize) -> DevName {
        let x = (b'a' + id as u8) as char;
        DevName::new(format!("sd{}", x), id)
    }

    #[allow(dead_code)]
    pub fn free_id(&self, id: usize) {
        if id >= Self::MAX_DEVICES {
            return;
        }
        self.inner().id_bmp.set(id, false);
        self.inner().devname[id] = None;
    }
}
