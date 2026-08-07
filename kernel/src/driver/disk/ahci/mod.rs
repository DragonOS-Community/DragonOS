//! AHCI PCI driver.
//!
//! AHCI controllers are discovered by the PCI bus.  An empty controller is a
//! valid device and remains bound without publishing a block device.

pub mod ahcidisk;
pub mod hba;

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;
use system_error::SystemError;
use unified_init::macros::unified_init;

use core::{
    mem::size_of,
    ptr::write_bytes,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use crate::{
    arch::MMArch,
    driver::{
        base::{
            block::manager::block_dev_manager,
            device::{
                bus::Bus,
                driver::{Driver, DriverCommonData},
                Device, IdTable,
            },
            kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
            kset::KSet,
        },
        pci::{
            dev_id::PciDeviceID,
            device::PciDevice,
            driver::{pci_driver_manager, PciDriver},
            pci::{Command, PciDeviceStructure, PciDeviceStructureGeneralDevice},
        },
    },
    filesystem::kernfs::KernFSInode,
    init::initcall::INITCALL_DEVICE,
    libs::{
        mutex::{Mutex, MutexGuard},
        rwlock::RwLock,
        rwsem::{RwSemReadGuard, RwSemWriteGuard},
        spinlock::SpinLock,
    },
    mm::{
        dma::{DmaAllocOptions, DmaBuffer, DmaDirection},
        MemoryManagementArch, PhysAddr,
    },
    time::{Duration, Instant},
};

use self::{
    ahcidisk::LockedAhciDisk,
    hba::{
        FisRegH2D, FisType, HbaCmdHeader, HbaCmdTable, HbaMem, HbaPort, HbaPortType,
        ATA_CMD_FLUSH_CACHE, ATA_CMD_FLUSH_CACHE_EXT, ATA_CMD_IDENTIFY, ATA_DEV_BUSY, ATA_DEV_DRQ,
        HBA_PORT_IS_ERR,
    },
};

pub(crate) struct AhciIdentify {
    capacity_lba: usize,
    flush_command: Option<u8>,
    reliable_flush: bool,
}

struct PendingAtaCommand {
    port_no: usize,
    slot: u32,
    issued: bool,
}

struct PendingIdentify {
    command: PendingAtaCommand,
    buffer: Option<DmaBuffer>,
    result: Option<Result<AhciIdentify, SystemError>>,
}

struct PendingFlush {
    command: PendingAtaCommand,
    result: Option<Result<(), SystemError>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AtaCommandStatus {
    Pending,
    Complete,
    Error,
}

const AHCI_CLASS_CODE: u32 = 0x010601;
const AHCI_CLASS_MASK: u32 = 0x00ff_ffff;
const AHCI_GHC_AE: u32 = 1 << 31;
const AHCI_GHC_HR: u32 = 1;
const AHCI_CAP2_BOH: u32 = 1;
const AHCI_BOHC_BOS: u32 = 1;
const AHCI_BOHC_OOS: u32 = 1 << 1;
const AHCI_BOHC_BB: u32 = 1 << 4;
const AHCI_CAP_SSS: u32 = 1 << 27;
const AHCI_CAP_CPD: u32 = 1 << 20;
const AHCI_PXCMD_CPD: u32 = 1 << 20;
const AHCI_PXCMD_ICC_MASK: u32 = 0xf << 28;
const AHCI_PXCMD_ICC_ACTIVE: u32 = 1 << 28;
const AHCI_PXCMD_POD: u32 = 1 << 2;
const AHCI_PXCMD_SUD: u32 = 1 << 1;
const AHCI_MAX_SECTORS: u64 = 1u64 << 48;
const AHCI_POLL_YIELD_INTERVAL: usize = 1 << 10;
const AHCI_LINK_TIMEOUT_MS: u64 = 2_000;
const AHCI_PORT_STOP_TIMEOUT_MS: u64 = 500;
const AHCI_COMMAND_TIMEOUT_MS: u64 = 30_000;
// 32 command lists (32 KiB) + 32 received-FIS areas (8 KiB) +
// 32 * 32 command tables (256 KiB), rounded to the allocator's power of two.
const AHCI_COMMAND_ARENA_SIZE: usize = 1 << 19;

const fn capacity_is_addressable(capacity: u64) -> bool {
    capacity != 0 && capacity <= AHCI_MAX_SECTORS
}

const fn poll_should_yield(iteration: usize) -> bool {
    iteration != 0 && iteration.is_multiple_of(AHCI_POLL_YIELD_INTERVAL)
}

const fn classify_command_status(port_is: u32, port_ci: u32, slot: u32) -> AtaCommandStatus {
    if port_is & HBA_PORT_IS_ERR != 0 {
        AtaCommandStatus::Error
    } else if port_ci & (1 << slot) == 0 {
        AtaCommandStatus::Complete
    } else {
        AtaCommandStatus::Pending
    }
}

fn read_command_status(port: &HbaPort, slot: u32) -> AtaCommandStatus {
    // Read CI first. If the device completes with an error between these two
    // MMIO reads, the later, latched PxIS read still makes the result fail.
    let port_ci = volatile_read!(port.ci);
    let port_is = volatile_read!(port.is);
    classify_command_status(port_is, port_ci, slot)
}

// These protocol boundaries are checked by every kernel build, including
// architectures that do not instantiate the x86-only AHCI driver.
const _: () = {
    assert!(!capacity_is_addressable(0));
    assert!(capacity_is_addressable(AHCI_MAX_SECTORS));
    assert!(!capacity_is_addressable(AHCI_MAX_SECTORS + 1));
    assert!(!poll_should_yield(0));
    assert!(!poll_should_yield(AHCI_POLL_YIELD_INTERVAL - 1));
    assert!(poll_should_yield(AHCI_POLL_YIELD_INTERVAL));
    assert!(matches!(
        classify_command_status(HBA_PORT_IS_ERR, 0, 0),
        AtaCommandStatus::Error
    ));
    assert!(matches!(
        classify_command_status(0, 0, 0),
        AtaCommandStatus::Complete
    ));
    assert!(matches!(
        classify_command_status(0, 1, 0),
        AtaCommandStatus::Pending
    ));
};

lazy_static! {
    /// DMA memory whose controller could not be stopped or reset. Entries are
    /// keyed by PCI device name and reclaimed after that controller next
    /// completes a successful HBA reset with Bus Master disabled.
    static ref AHCI_DMA_QUARANTINE: Mutex<HashMap<String, Vec<DmaBuffer>>> =
        Mutex::new(HashMap::new());
}

/// Resources owned by one bound PCI AHCI controller.
pub struct AhciController {
    device: Arc<dyn PciDevice>,
    pci: Arc<PciDeviceStructureGeneralDevice>,
    abar: usize,
    port_count: usize,
    original_command: Command,
    dma_mask: u64,
    command_slots: u8,
    command_memory: Mutex<Option<DmaBuffer>>,
    disks: SpinLock<Vec<Weak<LockedAhciDisk>>>,
    accepting_io: AtomicBool,
    failed_ports: AtomicU32,
    quarantined_dma: SpinLock<Vec<DmaBuffer>>,
    hardware_stopped: AtomicBool,
    detached: AtomicBool,
    port_locks: Vec<Mutex<()>>,
    lifecycle: Mutex<ControllerState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControllerState {
    Running,
    Detaching,
    RemovalFailed,
    Stopping,
    Stopped,
}

impl core::fmt::Debug for AhciController {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AhciController")
            .field("bdf", &self.pci.common_header.bus_device_function)
            .field("abar", &self.abar)
            .field("port_count", &self.port_count)
            .field("command_slots", &self.command_slots)
            .field("has_command_memory", &self.command_memory.lock().is_some())
            .finish()
    }
}

impl AhciController {
    fn probe(
        device: Arc<dyn PciDevice>,
        pci: Arc<PciDeviceStructureGeneralDevice>,
    ) -> Result<Arc<Self>, SystemError> {
        let original_command = pci.status_command().1;
        // Preserve firmware's Bus Master state until BIOS/OS handoff finishes;
        // clearing it earlier can prevent firmware transactions from draining.
        pci.set_command(original_command | Command::MEMORY_SPACE);
        let result = (|| {
            if pci.bar_ioremap().ok_or(SystemError::EINVAL)?.is_err() {
                return Err(SystemError::EIO);
            }

            let bars = pci.bar().ok_or(SystemError::EACCES)?;
            let bars = bars.read();
            let abar = bars
                .get_bar(5)
                .map_err(|_| SystemError::EACCES)?
                .virtual_address_at(0, 0x100)
                .ok_or(SystemError::EACCES)?
                .data();
            drop(bars);
            let hba = abar as *mut HbaMem;
            Self::initialize_hba(hba, &pci)?;
            Self::reclaim_quarantined_dma(&device.name());

            let cap = volatile_read!((*hba).cap);
            let pi = volatile_read!((*hba).pi);
            let pi_ports = if pi == 0 {
                0
            } else {
                32 - pi.leading_zeros() as usize
            };
            let port_count = (((cap & 0x1f) + 1) as usize).max(pi_ports).min(32);
            let required_len = 0x100usize
                .checked_add(
                    port_count
                        .checked_mul(size_of::<HbaPort>())
                        .ok_or(SystemError::EOVERFLOW)?,
                )
                .ok_or(SystemError::EOVERFLOW)?;
            let bars = pci.bar().ok_or(SystemError::EACCES)?;
            if bars
                .read()
                .get_bar(5)
                .map_err(|_| SystemError::EACCES)?
                .virtual_address_at(0, required_len)
                .is_none()
            {
                return Err(SystemError::EACCES);
            }
            let mut online_ports = Vec::new();
            let mut training_ports = Vec::new();
            for port_no in 0..port_count {
                if pi & (1 << port_no) == 0 {
                    continue;
                }
                let port = unsafe {
                    core::ptr::addr_of_mut!((*hba).ports)
                        .cast::<HbaPort>()
                        .add(port_no)
                };
                let mut cmd = volatile_read!((*port).cmd);
                if cap & AHCI_CAP_SSS != 0 {
                    cmd |= AHCI_PXCMD_SUD;
                }
                if cap & AHCI_CAP_CPD != 0 && cmd & AHCI_PXCMD_CPD != 0 {
                    cmd |= AHCI_PXCMD_POD;
                }
                cmd = (cmd & !AHCI_PXCMD_ICC_MASK) | AHCI_PXCMD_ICC_ACTIVE;
                volatile_write!((*port).cmd, cmd);
                let _ = volatile_read!((*port).cmd);
            }
            if cap & (AHCI_CAP_SSS | AHCI_CAP_CPD) != 0 {
                let settle_until = Instant::now() + Duration::from_millis(100);
                while Instant::now() < settle_until {
                    core::hint::spin_loop();
                }
            }
            for port_no in 0..port_count {
                if pi & (1 << port_no) == 0 {
                    continue;
                }
                let port = unsafe {
                    core::ptr::addr_of_mut!((*hba).ports)
                        .cast::<HbaPort>()
                        .add(port_no)
                };
                match volatile_read!((*port).ssts) & 0xf {
                    3 => online_ports.push(port_no),
                    0 => {}
                    _ => training_ports.push(port_no),
                }
            }
            // Ready links are considered first, but all candidates train in
            // one interleaved window below rather than serial per-port waits.
            let candidate_ports = online_ports
                .into_iter()
                .chain(training_ports)
                .collect::<Vec<_>>();

            // Empty controllers are normal and do not need command DMA memory.
            let dma_mask = if cap & (1 << 31) != 0 {
                u64::MAX
            } else {
                u32::MAX.into()
            };
            let command_slots = (((cap >> 8) & 0x1f) + 1) as u8;
            let command_memory = if candidate_ports.is_empty() {
                None
            } else {
                let options = DmaAllocOptions {
                    dma_mask: Some(dma_mask),
                    use_pool: false,
                    ..Default::default()
                };
                let memory = DmaBuffer::try_alloc_bytes(AHCI_COMMAND_ARENA_SIZE, options)?;
                Some(memory)
            };

            let controller = Arc::new(Self {
                device,
                pci: pci.clone(),
                abar,
                port_count,
                original_command,
                dma_mask,
                command_slots,
                command_memory: Mutex::new(command_memory),
                disks: SpinLock::new(Vec::new()),
                accepting_io: AtomicBool::new(true),
                failed_ports: AtomicU32::new(0),
                // At most one command can be active on each serialized port.
                // Reserve all possible entries before any fault path so DMA
                // quarantine never allocates after a command-engine failure.
                quarantined_dma: SpinLock::new(Vec::with_capacity(32)),
                hardware_stopped: AtomicBool::new(false),
                detached: AtomicBool::new(false),
                port_locks: (0..32).map(|_| Mutex::new(())).collect(),
                lifecycle: Mutex::new(ControllerState::Running),
            });

            if !candidate_ports.is_empty() {
                let command = pci.status_command().1;
                pci.set_command(command | Command::BUS_MASTER);
            }
            let mut link_states = Vec::new();
            let mut provisional_ports = Vec::new();
            for port_no in candidate_ports {
                let base = controller
                    .command_memory
                    .lock()
                    .as_ref()
                    .ok_or(SystemError::ENOMEM)?
                    .paddr();
                let fb = base + (32 << 10) + (port_no << 8);
                // Keep every attempted port so even a partially failed setup
                // has its provisional FIS receiver stopped below.
                provisional_ports.push(port_no);
                match unsafe { &mut *controller.port_ptr(port_no) }.begin_link_reset(fb as u64) {
                    Ok(received_fis_vaddr) => {
                        link_states.push((port_no, received_fis_vaddr, None, None));
                    }
                    Err(err) => {
                        controller
                            .failed_ports
                            .fetch_or(1 << port_no, Ordering::AcqRel);
                        log::warn!(
                            "AHCI {} port {} link initialization failed: {:?}",
                            controller.pci.common_header.bus_device_function,
                            port_no,
                            err
                        );
                    }
                }
            }

            // AHCI requires COMRESET to remain asserted for at least 1 ms.
            // Asserting every port first lets all links recover concurrently.
            let assert_until = Instant::now() + Duration::from_millis(1);
            while Instant::now() < assert_until {
                core::hint::spin_loop();
            }
            for port_no in &provisional_ports {
                unsafe { &mut *controller.port_ptr(*port_no) }.finish_link_reset_assertion();
            }

            let link_deadline = Instant::now() + Duration::from_millis(AHCI_LINK_TIMEOUT_MS);
            let mut link_iteration = 0usize;
            loop {
                let mut pending = false;
                for (port_no, received_fis_vaddr, stable_since, port_type) in &mut link_states {
                    if port_type.is_some() {
                        continue;
                    }
                    *port_type = unsafe { &mut *controller.port_ptr(*port_no) }
                        .classify_reset_link(*received_fis_vaddr, stable_since);
                    pending |= port_type.is_none();
                }
                if !pending || Instant::now() >= link_deadline {
                    break;
                }
                if poll_should_yield(link_iteration) {
                    crate::sched::sched_yield();
                } else {
                    core::hint::spin_loop();
                }
                link_iteration = link_iteration.wrapping_add(1);
            }

            for (port_no, _, _, port_type) in &link_states {
                if port_type.is_none() {
                    controller
                        .failed_ports
                        .fetch_or(1 << *port_no, Ordering::AcqRel);
                    log::warn!(
                        "AHCI {} port {} link initialization timed out",
                        controller.pci.common_header.bus_device_function,
                        port_no
                    );
                }
            }

            // Stop all temporary FIS receivers concurrently.  A successful
            // classification remains valid because the normal path does not
            // reset the links again before IDENTIFY.
            if controller
                .stop_ports_concurrently(&provisional_ports)
                .is_err()
            {
                // Do not publish using classifications obtained before a
                // recovery reset.  Reset only proves DMA is stopped; this
                // probe fails and a later probe must classify afresh.
                let command = pci.status_command().1;
                pci.set_command(command & !Command::BUS_MASTER);
                if Self::reset_hba(hba).is_ok() {
                    controller.hardware_stopped.store(true, Ordering::Release);
                }
                return Err(SystemError::ETIMEDOUT);
            }

            let has_sata = link_states
                .iter()
                .any(|(_, _, _, port_type)| *port_type == Some(HbaPortType::Sata));
            if has_sata {
                let command = pci.status_command().1;
                pci.set_command(command | Command::BUS_MASTER);
            }
            let sata_ports = link_states
                .into_iter()
                .filter_map(|(port_no, _, _, port_type)| {
                    (port_type == Some(HbaPortType::Sata)).then_some(port_no)
                })
                .collect::<Vec<_>>();
            controller.probe_sata_ports(&sata_ports);
            if controller.disks.lock().is_empty() {
                let command = pci.status_command().1;
                pci.set_command(command & !Command::BUS_MASTER);
            }
            Ok(controller)
        })();
        if result.is_err() {
            pci.set_command(original_command & !Command::BUS_MASTER);
        }
        result
    }

    fn initialize_hba(
        hba: *mut HbaMem,
        pci: &PciDeviceStructureGeneralDevice,
    ) -> Result<(), SystemError> {
        if volatile_read!((*hba).cap2) & AHCI_CAP2_BOH != 0 {
            let bohc = volatile_read!((*hba).bohc);
            volatile_write!((*hba).bohc, bohc | AHCI_BOHC_OOS);
            let deadline = Instant::now() + Duration::from_secs(2);
            while volatile_read!((*hba).bohc) & (AHCI_BOHC_BOS | AHCI_BOHC_BB) != 0 {
                if Instant::now() >= deadline {
                    return Err(SystemError::ETIMEDOUT);
                }
                core::hint::spin_loop();
            }
        }

        // Firmware ownership is now released. Disable old DMA before reset
        // invalidates firmware command-list addresses.
        let command = pci.status_command().1;
        pci.set_command(command & !Command::BUS_MASTER);

        Self::reset_hba(hba)
    }

    fn reset_hba(hba: *mut HbaMem) -> Result<(), SystemError> {
        let ghc = volatile_read!((*hba).ghc);
        if ghc == u32::MAX {
            return Err(SystemError::ENODEV);
        }
        volatile_write!((*hba).ghc, ghc | AHCI_GHC_AE);
        if volatile_read!((*hba).ghc) & AHCI_GHC_AE == 0 {
            return Err(SystemError::EIO);
        }
        let ghc = volatile_read!((*hba).ghc);
        volatile_write!((*hba).ghc, ghc | AHCI_GHC_HR);
        let _ = volatile_read!((*hba).ghc);
        let deadline = Instant::now() + Duration::from_secs(1);
        while volatile_read!((*hba).ghc) & AHCI_GHC_HR != 0 {
            if Instant::now() >= deadline {
                return Err(SystemError::ETIMEDOUT);
            }
            core::hint::spin_loop();
        }
        let ghc = volatile_read!((*hba).ghc);
        volatile_write!((*hba).ghc, ghc | AHCI_GHC_AE);
        if volatile_read!((*hba).ghc) & AHCI_GHC_AE == 0 {
            return Err(SystemError::EIO);
        }
        Ok(())
    }

    fn reclaim_quarantined_dma(device_name: &str) {
        let retired = AHCI_DMA_QUARANTINE.lock().remove(device_name);
        // Do not run the DMA allocator while holding the quarantine mutex.
        drop(retired);
    }

    fn initialize_port(&self, port_no: usize) -> Result<(), SystemError> {
        let base = self
            .command_memory
            .lock()
            .as_ref()
            .ok_or(SystemError::ENOMEM)?
            .paddr();
        let fb = base + (32 << 10) + (port_no << 8);
        let clb = base + (port_no << 10);
        let ctbas = (0..32)
            .map(|slot| (base + (40 << 10) + (port_no << 13) + (slot << 8)) as u64)
            .collect::<Vec<_>>();

        unsafe { &mut *self.port_ptr(port_no) }.init(clb as u64, fb as u64, &ctbas)
    }

    fn publish_identified(
        self: &Arc<Self>,
        port_no: usize,
        identify: AhciIdentify,
    ) -> Result<(), SystemError> {
        let result = (|| {
            let disk = LockedAhciDisk::new(self.clone(), port_no as u8, identify)?;
            block_dev_manager().register(disk.clone())?;
            self.disks.lock().push(Arc::downgrade(&disk));
            Ok(())
        })();
        if result.is_err() {
            self.failed_ports.fetch_or(1 << port_no, Ordering::AcqRel);
            let _ = unsafe { &mut *self.port_ptr(port_no) }.stop();
        }
        result
    }

    pub(crate) fn port_ptr(&self, port_no: usize) -> *mut HbaPort {
        debug_assert!(port_no < self.port_count);
        unsafe {
            core::ptr::addr_of_mut!((*(self.abar as *mut HbaMem)).ports)
                .cast::<HbaPort>()
                .add(port_no)
        }
    }

    pub(crate) fn lock_port_for_io(
        &self,
        port_no: usize,
    ) -> Result<MutexGuard<'_, ()>, SystemError> {
        if !self.accepting_io.load(Ordering::Acquire) {
            return Err(SystemError::ESHUTDOWN);
        }
        if self.failed_ports.load(Ordering::Acquire) & (1 << port_no) != 0 {
            return Err(SystemError::EIO);
        }
        let guard = self.port_locks[port_no].lock();
        if !self.accepting_io.load(Ordering::Acquire) {
            drop(guard);
            return Err(SystemError::ESHUTDOWN);
        }
        if self.failed_ports.load(Ordering::Acquire) & (1 << port_no) != 0 {
            drop(guard);
            return Err(SystemError::EIO);
        }
        Ok(guard)
    }

    pub(crate) fn acquire_holder(&self) -> Result<(), SystemError> {
        let state = self.lifecycle.lock();
        if *state != ControllerState::Running || !self.accepting_io.load(Ordering::Acquire) {
            return Err(SystemError::ENODEV);
        }
        Ok(())
    }

    /// Freeze one failed port. A controller-wide Bus Master shutdown would
    /// interrupt unrelated ports, so a buffer which may still be a DMA target
    /// remains owned by this controller until teardown stops or resets the HBA.
    pub(crate) fn abort_failed_command(&self, port_no: usize, buffer: Option<DmaBuffer>) {
        self.failed_ports.fetch_or(1 << port_no, Ordering::AcqRel);
        if unsafe { &mut *self.port_ptr(port_no) }.stop().is_err() {
            if let Some(buffer) = buffer {
                self.quarantined_dma.lock().push(buffer);
            }
        } else {
            drop(buffer);
        }
    }

    pub(crate) fn allocate_dma(
        &self,
        size: usize,
        direction: DmaDirection,
    ) -> Result<DmaBuffer, SystemError> {
        let buffer = DmaBuffer::try_alloc_bytes(
            size,
            DmaAllocOptions {
                direction,
                dma_mask: Some(self.dma_mask),
                ..Default::default()
            },
        )?;
        Ok(buffer)
    }

    fn prepare_identify(&self, port_no: usize) -> Result<PendingIdentify, SystemError> {
        let port = unsafe { &mut *self.port_ptr(port_no) };
        let identify = self.allocate_dma(512, DmaDirection::FromDevice)?;
        let slot = port
            .find_cmdslot(self.command_slots)
            .ok_or(SystemError::EBUSY)?;
        volatile_write!(port.is, u32::MAX);

        let clb = volatile_read!(port.clb);
        let header = unsafe {
            &mut *(MMArch::phys_2_virt(PhysAddr::new(
                clb as usize + slot as usize * size_of::<HbaCmdHeader>(),
            ))
            .ok_or(SystemError::EFAULT)?
            .data() as *mut HbaCmdHeader)
        };
        let ctba = volatile_read!(header.ctba);
        let table = unsafe {
            &mut *(MMArch::phys_2_virt(PhysAddr::new(ctba as usize))
                .ok_or(SystemError::EFAULT)?
                .data() as *mut HbaCmdTable)
        };
        unsafe { write_bytes(table, 0, 1) };
        volatile_write!(
            header.cfl,
            (size_of::<FisRegH2D>() / size_of::<u32>()) as u8
        );
        volatile_write!(header._pm, 0);
        volatile_write!(header._prdbc, 0);
        volatile_write!(header.prdtl, 1);
        volatile_write!(table.prdt_entry[0].dba, identify.paddr() as u64);
        volatile_write!(table.prdt_entry[0].dbc, 511);
        let fis = unsafe { &mut *(table.cfis.as_mut_ptr() as *mut FisRegH2D) };
        volatile_write!(fis.fis_type, FisType::RegH2D as u8);
        volatile_write!(fis.pm, 1 << 7);
        volatile_write!(fis.command, ATA_CMD_IDENTIFY);

        Ok(PendingIdentify {
            command: PendingAtaCommand {
                port_no,
                slot,
                issued: false,
            },
            buffer: Some(identify),
            result: None,
        })
    }

    fn parse_identify(mut identify: DmaBuffer) -> Result<AhciIdentify, SystemError> {
        let bytes = identify.as_mut_slice();
        let word = |index: usize| u16::from_le_bytes([bytes[index * 2], bytes[index * 2 + 1]]);
        if word(49) & (1 << 8) == 0 {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let word83 = word(83);
        if word83 & 0xc000 != 0x4000 || word83 & (1 << 10) == 0 {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let logical_sector_size = if word(106) & 0xd000 == 0x5000 {
            (u32::from(word(118)) << 16 | u32::from(word(117)))
                .checked_mul(2)
                .ok_or(SystemError::EOVERFLOW)?
        } else {
            512
        };
        if logical_sector_size != 512 {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let capacity = u64::from(word(100))
            | u64::from(word(101)) << 16
            | u64::from(word(102)) << 32
            | u64::from(word(103)) << 48;
        // READ/WRITE DMA EXT carries a 48-bit LBA, so at most 2^48 sectors
        // (ending at LBA 2^48 - 1) are addressable. A larger IDENTIFY value
        // would pass the range check but be truncated while building the FIS.
        if !capacity_is_addressable(capacity) {
            return Err(if capacity == 0 {
                SystemError::EIO
            } else {
                SystemError::EOVERFLOW
            });
        }
        let capacity_lba = usize::try_from(capacity).map_err(|_| SystemError::EOVERFLOW)?;
        let has_flush_ext = word83 & (1 << 13) != 0;
        let has_flush = word83 & (1 << 12) != 0;
        let word87 = word(87);
        let write_cache_enabled = word87 & 0xc000 == 0x4000 && word(85) & (1 << 5) != 0;
        // Linux also attempts the base FLUSH CACHE command when write cache is
        // enabled but the capability bits are absent.  This makes a broken or
        // incomplete IDENTIFY response fail visibly instead of silently
        // treating volatile cached writes as synchronized.
        let flush_command = if has_flush_ext {
            Some(ATA_CMD_FLUSH_CACHE_EXT)
        } else if has_flush || write_cache_enabled {
            Some(ATA_CMD_FLUSH_CACHE)
        } else {
            None
        };
        Ok(AhciIdentify {
            capacity_lba,
            flush_command,
            reliable_flush: has_flush_ext || has_flush,
        })
    }

    /// Advance one already prepared command without waiting on another port.
    /// This keeps controller-wide probe and teardown budgets fair when one
    /// device remains BUSY forever.
    fn advance_prepared_command(
        &self,
        command: &mut PendingAtaCommand,
    ) -> Result<bool, SystemError> {
        let port = unsafe { &mut *self.port_ptr(command.port_no) };
        if read_command_status(port, command.slot) == AtaCommandStatus::Error {
            return Err(SystemError::EIO);
        }
        if !command.issued {
            if volatile_read!(port.tfd) as u8 & (ATA_DEV_BUSY | ATA_DEV_DRQ) != 0 {
                return Ok(false);
            }
            core::sync::atomic::compiler_fence(Ordering::Release);
            let ci = volatile_read!(port.ci);
            volatile_write!(port.ci, ci | (1 << command.slot));
            command.issued = true;
        }
        match read_command_status(port, command.slot) {
            AtaCommandStatus::Error => return Err(SystemError::EIO),
            AtaCommandStatus::Complete => {
                core::sync::atomic::compiler_fence(Ordering::Acquire);
                return Ok(true);
            }
            AtaCommandStatus::Pending => {}
        }
        Ok(false)
    }

    fn fail_pending_identify(pending: &mut PendingIdentify, err: SystemError) {
        pending.result = Some(Err(err));
    }

    fn stop_failed_batch(&self, ports: &[usize]) -> bool {
        let failed_mask = ports.iter().fold(0, |mask, port_no| mask | 1 << port_no);
        self.failed_ports.fetch_or(failed_mask, Ordering::AcqRel);
        self.stop_ports_concurrently(ports).is_ok()
    }

    /// Prepare every SATA port before polling any of them. All ports therefore
    /// receive a chance to complete IDENTIFY within one controller budget.
    fn probe_sata_ports(self: &Arc<Self>, ports: &[usize]) {
        let mut pending = Vec::with_capacity(ports.len());
        let mut setup_failed = Vec::new();
        for &port_no in ports {
            let prepared = self
                .initialize_port(port_no)
                .and_then(|()| self.prepare_identify(port_no));
            match prepared {
                Ok(command) => pending.push(command),
                Err(err) => {
                    setup_failed.push(port_no);
                    log::error!(
                        "AHCI {} port {} initialization failed: {:?}",
                        self.pci.common_header.bus_device_function,
                        port_no,
                        err
                    );
                }
            }
        }
        if !setup_failed.is_empty() {
            self.stop_failed_batch(&setup_failed);
        }

        // Link reset and the concurrent provisional stop already established
        // the setup bound. Start the IDENTIFY command budget only after every
        // usable port is prepared, so setup failure on a low-numbered port
        // cannot consume a healthy higher-numbered port's command window.
        let deadline = Instant::now() + Duration::from_millis(AHCI_COMMAND_TIMEOUT_MS);
        let mut remaining = pending.len();
        let mut iteration = 0usize;
        while remaining != 0 && Instant::now() < deadline {
            for item in &mut pending {
                if item.result.is_some() {
                    continue;
                }
                match self.advance_prepared_command(&mut item.command) {
                    Ok(false) => {}
                    Ok(true) => {
                        let result = item
                            .buffer
                            .take()
                            .ok_or(SystemError::EIO)
                            .and_then(Self::parse_identify);
                        if result.is_err() {
                            self.failed_ports
                                .fetch_or(1 << item.command.port_no, Ordering::AcqRel);
                        }
                        item.result = Some(result);
                        remaining -= 1;
                    }
                    Err(err) => {
                        Self::fail_pending_identify(item, err);
                        remaining -= 1;
                    }
                }
            }
            if remaining != 0 {
                if poll_should_yield(iteration) {
                    crate::sched::sched_yield();
                } else {
                    core::hint::spin_loop();
                }
                iteration = iteration.wrapping_add(1);
            }
        }
        // A scheduler yield may return after the deadline even though hardware
        // completed before it. Observe already-issued commands once more, but
        // never submit a previously BUSY command after its budget expired.
        for item in &mut pending {
            if item.result.is_some() || !item.command.issued {
                continue;
            }
            let port = unsafe { &*self.port_ptr(item.command.port_no) };
            match read_command_status(port, item.command.slot) {
                AtaCommandStatus::Complete => {
                    core::sync::atomic::compiler_fence(Ordering::Acquire);
                    let result = item
                        .buffer
                        .take()
                        .ok_or(SystemError::EIO)
                        .and_then(Self::parse_identify);
                    item.result = Some(result);
                }
                AtaCommandStatus::Error => item.result = Some(Err(SystemError::EIO)),
                AtaCommandStatus::Pending => {}
            }
        }
        for item in &mut pending {
            if item.result.is_none() {
                Self::fail_pending_identify(item, SystemError::ETIMEDOUT);
            }
        }

        let failed = pending
            .iter()
            .filter_map(|item| {
                item.result
                    .as_ref()
                    .is_some_and(Result::is_err)
                    .then_some(item.command.port_no)
            })
            .collect::<Vec<_>>();
        if !failed.is_empty() && !self.stop_failed_batch(&failed) {
            let mut quarantine = self.quarantined_dma.lock();
            for item in &mut pending {
                if item.command.issued && item.result.as_ref().is_some_and(Result::is_err) {
                    if let Some(buffer) = item.buffer.take() {
                        quarantine.push(buffer);
                    }
                }
            }
        }

        for item in pending {
            let port_no = item.command.port_no;
            match item.result.unwrap_or(Err(SystemError::EIO)) {
                Ok(identify) => {
                    if let Err(err) = self.publish_identified(port_no, identify) {
                        log::error!(
                            "AHCI {} port {} publication failed: {:?}",
                            self.pci.common_header.bus_device_function,
                            port_no,
                            err
                        );
                    }
                }
                Err(err) => log::error!(
                    "AHCI {} port {} IDENTIFY failed: {:?}",
                    self.pci.common_header.bus_device_function,
                    port_no,
                    err
                ),
            }
        }
    }

    fn prepare_flush_command(
        &self,
        port_no: usize,
        command: u8,
    ) -> Result<PendingAtaCommand, SystemError> {
        let port = unsafe { &mut *self.port_ptr(port_no) };
        volatile_write!(port.is, u32::MAX);
        let slot = port
            .find_cmdslot(self.command_slots)
            .ok_or(SystemError::EBUSY)?;
        let clb = volatile_read!(port.clb);
        let header = unsafe {
            &mut *(MMArch::phys_2_virt(PhysAddr::new(
                clb as usize + slot as usize * size_of::<HbaCmdHeader>(),
            ))
            .ok_or(SystemError::EFAULT)?
            .data() as *mut HbaCmdHeader)
        };
        let ctba = volatile_read!(header.ctba);
        let table = unsafe {
            &mut *(MMArch::phys_2_virt(PhysAddr::new(ctba as usize))
                .ok_or(SystemError::EFAULT)?
                .data() as *mut HbaCmdTable)
        };
        unsafe { write_bytes(table, 0, 1) };
        volatile_write!(
            header.cfl,
            (size_of::<FisRegH2D>() / size_of::<u32>()) as u8
        );
        volatile_write!(header._pm, 0);
        volatile_write!(header._prdbc, 0);
        volatile_write!(header.prdtl, 0);
        let fis = unsafe { &mut *(table.cfis.as_mut_ptr() as *mut FisRegH2D) };
        volatile_write!(fis.fis_type, FisType::RegH2D as u8);
        volatile_write!(fis.pm, 1 << 7);
        volatile_write!(fis.command, command);
        Ok(PendingAtaCommand {
            port_no,
            slot,
            issued: false,
        })
    }

    pub(crate) fn flush_port(&self, port_no: usize, command: u8) -> Result<(), SystemError> {
        let _guard = self.lock_port_for_io(port_no)?;
        let mut pending = self.prepare_flush_command(port_no, command)?;
        let port = unsafe { &mut *self.port_ptr(port_no) };
        Self::wait_tfd_ready(port)?;
        core::sync::atomic::compiler_fence(Ordering::Release);
        let ci = volatile_read!(port.ci);
        volatile_write!(port.ci, ci | (1 << pending.slot));
        pending.issued = true;
        if let Err(err) = Self::wait_slot(port, pending.slot) {
            self.abort_failed_command(port_no, None);
            return Err(err);
        }
        core::sync::atomic::compiler_fence(Ordering::Acquire);
        Ok(())
    }

    pub(crate) fn wait_tfd_ready(port: &HbaPort) -> Result<(), SystemError> {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut iteration = 0usize;
        loop {
            if volatile_read!(port.tfd) as u8 & (ATA_DEV_BUSY | ATA_DEV_DRQ) == 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(SystemError::ETIMEDOUT);
            }
            if poll_should_yield(iteration) {
                crate::sched::sched_yield();
            } else {
                core::hint::spin_loop();
            }
            iteration = iteration.wrapping_add(1);
        }
    }

    pub(crate) fn wait_slot(port: &HbaPort, slot: u32) -> Result<(), SystemError> {
        let deadline = Instant::now() + Duration::from_millis(AHCI_COMMAND_TIMEOUT_MS);
        let mut iteration = 0usize;
        loop {
            match read_command_status(port, slot) {
                AtaCommandStatus::Error => return Err(SystemError::EIO),
                AtaCommandStatus::Complete => return Ok(()),
                AtaCommandStatus::Pending => {}
            }
            if Instant::now() >= deadline {
                return Err(SystemError::ETIMEDOUT);
            }
            if poll_should_yield(iteration) {
                crate::sched::sched_yield();
            } else {
                core::hint::spin_loop();
            }
            iteration = iteration.wrapping_add(1);
        }
    }

    fn quiesce(&self) {
        self.accepting_io.store(false, Ordering::Release);
        // Taking each port lock waits for an already submitted command to
        // finish before the command engine is stopped.
        for lock in &self.port_locks {
            drop(lock.lock());
        }
    }

    fn hba_accessible(&self) -> bool {
        let hba = self.abar as *mut HbaMem;
        // GHC contains reserved-zero bits, so an all-ones read is a reliable
        // sign that the BAR no longer responds.  PI is a full 32-bit bitmap:
        // all ones is valid for a controller implementing every port.
        volatile_read!((*hba).ghc) != u32::MAX
    }

    /// Stop every listed command/FIS engine within one controller-wide
    /// deadline. Each scan advances all ports, so a stuck port cannot make
    /// later ports wait for a separate timeout.
    fn stop_ports_concurrently(&self, ports: &[usize]) -> Result<(), SystemError> {
        for port_no in ports {
            unsafe { &mut *self.port_ptr(*port_no) }.begin_provisional_stop();
        }
        let mut iteration = 0usize;
        let mut deadline = Instant::now() + Duration::from_millis(AHCI_PORT_STOP_TIMEOUT_MS);
        let mut stopping_fis_receive = false;
        loop {
            let mut all_stopped = true;
            for port_no in ports {
                // Intentionally non-short-circuiting: advance every port on
                // every scan within each controller-wide phase.
                let port = unsafe { &mut *self.port_ptr(*port_no) };
                all_stopped &= if stopping_fis_receive {
                    port.fis_receive_stopped()
                } else {
                    port.provisional_command_stopped()
                };
            }
            if all_stopped {
                if stopping_fis_receive {
                    return Ok(());
                }
                for port_no in ports {
                    unsafe { &mut *self.port_ptr(*port_no) }.begin_fis_receive_stop();
                }
                stopping_fis_receive = true;
                deadline = Instant::now() + Duration::from_millis(AHCI_PORT_STOP_TIMEOUT_MS);
                continue;
            }
            if Instant::now() >= deadline {
                return Err(SystemError::ETIMEDOUT);
            }
            if poll_should_yield(iteration) {
                crate::sched::sched_yield();
            } else {
                core::hint::spin_loop();
            }
            iteration = iteration.wrapping_add(1);
        }
    }

    fn stop_quiesced(&self) -> Result<(), SystemError> {
        let pi = volatile_read!((*(self.abar as *mut HbaMem)).pi);
        let ports = (0..self.port_count)
            .filter(|port_no| pi & (1 << port_no) != 0)
            .collect::<Vec<_>>();
        if let Err(err) = self.stop_ports_concurrently(&ports) {
            let command = self.pci.status_command().1;
            self.pci.set_command(command & !Command::BUS_MASTER);
            if Self::reset_hba(self.abar as *mut HbaMem).is_err() {
                Err(err)
            } else {
                self.hardware_stopped.store(true, Ordering::Release);
                self.release_quarantined_dma();
                Ok(())
            }
        } else {
            self.hardware_stopped.store(true, Ordering::Release);
            self.release_quarantined_dma();
            Ok(())
        }
    }

    fn release_quarantined_dma(&self) {
        let mut guard = self.quarantined_dma.lock();
        let retired = core::mem::take(&mut *guard);
        drop(guard);
        drop(retired);
    }

    fn flush_disks(&self) -> Result<(), SystemError> {
        if self.hardware_stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let disks = self
            .disks
            .lock()
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        if !self.pci.status_command().1.contains(Command::BUS_MASTER) {
            return if disks.iter().any(|disk| disk.needs_flush()) {
                Err(SystemError::EIO)
            } else {
                Ok(())
            };
        }
        let mut first_error = None;
        let mut pending = Vec::with_capacity(disks.len());
        let mut failed = Vec::new();
        for disk in disks {
            let Some((port_no, command)) = disk.teardown_flush_command() else {
                continue;
            };
            if self.failed_ports.load(Ordering::Acquire) & (1 << port_no) != 0 {
                first_error.get_or_insert(SystemError::EIO);
                failed.push(port_no);
                log::warn!(
                    "AHCI {} port {} teardown FLUSH skipped: port already failed",
                    self.pci.common_header.bus_device_function,
                    port_no
                );
                continue;
            }
            match self.prepare_flush_command(port_no, command) {
                Ok(command) => pending.push(PendingFlush {
                    command,
                    result: None,
                }),
                Err(err) => {
                    failed.push(port_no);
                    first_error.get_or_insert(err.clone());
                    log::warn!(
                        "AHCI {} port {} teardown FLUSH preparation failed: {:?}",
                        self.pci.common_header.bus_device_function,
                        port_no,
                        err
                    );
                }
            }
        }

        let deadline = Instant::now() + Duration::from_millis(AHCI_COMMAND_TIMEOUT_MS);
        let mut remaining = pending.len();
        let mut iteration = 0usize;
        while remaining != 0 && Instant::now() < deadline {
            for item in &mut pending {
                if item.result.is_some() {
                    continue;
                }
                match self.advance_prepared_command(&mut item.command) {
                    Ok(false) => {}
                    Ok(true) => {
                        item.result = Some(Ok(()));
                        remaining -= 1;
                    }
                    Err(err) => {
                        item.result = Some(Err(err));
                        remaining -= 1;
                    }
                }
            }
            if remaining != 0 {
                if poll_should_yield(iteration) {
                    crate::sched::sched_yield();
                } else {
                    core::hint::spin_loop();
                }
                iteration = iteration.wrapping_add(1);
            }
        }
        for item in &mut pending {
            if item.result.is_some() || !item.command.issued {
                continue;
            }
            let port = unsafe { &*self.port_ptr(item.command.port_no) };
            item.result = match read_command_status(port, item.command.slot) {
                AtaCommandStatus::Complete => {
                    core::sync::atomic::compiler_fence(Ordering::Acquire);
                    Some(Ok(()))
                }
                AtaCommandStatus::Error => Some(Err(SystemError::EIO)),
                AtaCommandStatus::Pending => None,
            };
        }
        for item in &mut pending {
            if item.result.is_none() {
                item.result = Some(Err(SystemError::ETIMEDOUT));
            }
            if let Some(Err(err)) = &item.result {
                first_error.get_or_insert(err.clone());
                failed.push(item.command.port_no);
                log::warn!(
                    "AHCI {} port {} teardown FLUSH failed: {:?}",
                    self.pci.common_header.bus_device_function,
                    item.command.port_no,
                    err
                );
            }
        }
        if !failed.is_empty() {
            self.stop_failed_batch(&failed);
        }
        first_error.map_or(Ok(()), Err)
    }

    fn unregister_disks(&self) {
        let disks = self
            .disks
            .lock()
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        for disk in disks {
            block_dev_manager().unregister_detached(
                &(disk as Arc<dyn crate::driver::base::block::block_device::BlockDevice>),
            );
        }
    }

    /// Perform the final PCI access and transfer all DMA ownership before any
    /// stale reference is allowed to outlive this controller generation.
    fn finalize_detach(&self, safe_to_release_dma: bool) {
        let command = self.pci.status_command().1;
        self.pci.set_command(command & !Command::BUS_MASTER);
        self.detached.store(true, Ordering::Release);
        self.retire_dma(safe_to_release_dma);
    }

    fn retire_dma(&self, safe_to_release: bool) {
        let mut local_guard = self.quarantined_dma.lock();
        let mut retired = core::mem::take(&mut *local_guard);
        drop(local_guard);
        if let Some(memory) = self.command_memory.lock().take() {
            retired.push(memory);
        }
        if safe_to_release || retired.is_empty() {
            drop(retired);
            return;
        }
        AHCI_DMA_QUARANTINE
            .lock()
            .entry(self.device.name())
            .or_default()
            .extend(retired);
    }
}

impl Drop for AhciController {
    fn drop(&mut self) {
        if self.detached.load(Ordering::Acquire) {
            return;
        }
        if self.hardware_stopped.load(Ordering::Acquire) {
            self.pci
                .set_command(self.original_command & !Command::BUS_MASTER);
            return;
        }
        self.quiesce();
        let _ = self.flush_disks();
        if self.stop_quiesced().is_err() {
            self.pci
                .set_command(self.original_command & !Command::BUS_MASTER);
            self.retire_dma(false);
            return;
        }
        // A detached storage controller must not regain DMA permission merely
        // because firmware happened to leave Bus Master set before binding.
        self.pci
            .set_command(self.original_command & !Command::BUS_MASTER);
    }
}

#[derive(Debug)]
#[cast_to([sync] PciDriver)]
struct AhciPciDriver {
    driver_data: RwLock<DriverCommonData>,
    kobj_data: RwLock<KObjectCommonData>,
    kobj_state: LockedKObjectState,
    ids: RwLock<Vec<Arc<PciDeviceID>>>,
    // None reserves a BDF while probe is running, preventing duplicate probe.
    controllers: RwLock<HashMap<String, Option<Arc<AhciController>>>>,
}

impl AhciPciDriver {
    fn new() -> Self {
        Self {
            driver_data: RwLock::new(DriverCommonData::default()),
            kobj_data: RwLock::new(KObjectCommonData::default()),
            kobj_state: LockedKObjectState::new(None),
            ids: RwLock::new(vec![Arc::new(PciDeviceID::class_match(
                AHCI_CLASS_CODE,
                AHCI_CLASS_MASK,
            ))]),
            controllers: RwLock::new(HashMap::new()),
        }
    }
}

impl PciDriver for AhciPciDriver {
    fn probe(&self, device: &Arc<dyn PciDevice>, _id: &PciDeviceID) -> Result<(), SystemError> {
        let name = device.name();
        {
            let mut controllers = self.controllers.write();
            if controllers.contains_key(&name) {
                return Err(SystemError::EBUSY);
            }
            controllers.insert(name.clone(), None);
        }
        let result = device
            .standard_device()
            .ok_or(SystemError::EINVAL)
            .and_then(|pci| AhciController::probe(device.clone(), pci));
        let mut controllers = self.controllers.write();
        match result {
            Ok(controller) => {
                controllers.insert(name, Some(controller));
                Ok(())
            }
            Err(err) => {
                controllers.remove(&name);
                Err(err)
            }
        }
    }

    fn remove(&self, device: &Arc<dyn PciDevice>) {
        let name = device.name();
        let Some(controller) = self.controllers.read().get(&name).and_then(Option::clone) else {
            return;
        };
        let mut state = controller.lifecycle.lock();
        if controller.detached.load(Ordering::Acquire) {
            self.controllers.write().remove(&name);
            return;
        }
        let already_stopped = *state == ControllerState::Stopped;
        if !already_stopped
            && !matches!(
                *state,
                ControllerState::Running | ControllerState::RemovalFailed
            )
        {
            log::warn!(
                "AHCI {} detach observed unexpected state {:?}",
                name,
                *state
            );
        }

        let mut flush_result = Ok(());
        let stop_result = if already_stopped {
            Ok(())
        } else {
            *state = ControllerState::Detaching;
            controller.quiesce();
            if controller.hba_accessible() {
                flush_result = controller.flush_disks();
                controller.stop_quiesced()
            } else {
                Err(SystemError::ENODEV)
            }
        };
        controller.unregister_disks();

        // This is the final PCI configuration access by this controller. The
        // detached flag makes delayed Drop from an open mount hardware-silent.
        controller.finalize_detach(stop_result.is_ok());
        *state = ControllerState::Stopped;
        self.controllers.write().remove(&name);
        if let Err(err) = flush_result {
            log::warn!("AHCI {} detach flush failed: {:?}", name, err);
        }
        if let Err(err) = stop_result {
            // The I/O gate is closed and all DMA allocations remain owned by
            // this controller or its keyed quarantine until a reset succeeds.
            log::warn!("AHCI {} command-engine stop failed: {:?}", name, err);
        }
    }

    fn shutdown(&self, device: &Arc<dyn PciDevice>) -> Result<(), SystemError> {
        if let Some(controller) = self
            .controllers
            .read()
            .get(&device.name())
            .and_then(Option::clone)
        {
            let mut state = controller.lifecycle.lock();
            if *state == ControllerState::Stopped {
                return Ok(());
            }
            if !matches!(
                *state,
                ControllerState::Running | ControllerState::RemovalFailed
            ) {
                return Err(SystemError::EBUSY);
            }
            *state = ControllerState::Stopping;
            controller.quiesce();
            let flush_result = controller.flush_disks();
            match controller.stop_quiesced() {
                Ok(()) => {
                    let command = controller.pci.status_command().1;
                    controller.pci.set_command(command & !Command::BUS_MASTER);
                    *state = ControllerState::Stopped;
                    flush_result?;
                }
                Err(err) => {
                    let command = controller.pci.status_command().1;
                    controller.pci.set_command(command & !Command::BUS_MASTER);
                    *state = ControllerState::RemovalFailed;
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    fn suspend(&self, _device: &Arc<dyn PciDevice>) -> Result<(), SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    fn resume(&self, _device: &Arc<dyn PciDevice>) -> Result<(), SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    fn add_dynid(&mut self, id: PciDeviceID) -> Result<(), SystemError> {
        self.ids.write().push(Arc::new(id));
        Ok(())
    }

    fn locked_dynid_list(&self) -> Option<Vec<Arc<PciDeviceID>>> {
        Some(self.ids.read().clone())
    }
}

impl Driver for AhciPciDriver {
    fn id_table(&self) -> Option<IdTable> {
        Some(IdTable::new("ahci".to_string(), None))
    }

    fn devices(&self) -> Vec<Arc<dyn Device>> {
        self.driver_data.read().devices.clone()
    }

    fn add_device(&self, device: Arc<dyn Device>) {
        self.driver_data.write().push_device(device);
    }

    fn delete_device(&self, device: &Arc<dyn Device>) {
        self.driver_data.write().delete_device(device);
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.driver_data.write().bus = bus;
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.driver_data.read().bus.clone()
    }
}

impl KObject for AhciPciDriver {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.kobj_data.write().kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.kobj_data.read().kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.kobj_data.read().parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.kobj_data.write().parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.kobj_data.read().kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.kobj_data.write().kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.kobj_data.read().kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.kobj_data.write().kobj_type = ktype;
    }

    fn name(&self) -> String {
        "ahci".to_string()
    }

    fn set_name(&self, _name: String) {}

    fn kobj_state(&self) -> RwSemReadGuard<'_, KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwSemWriteGuard<'_, KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state.write() = state;
    }
}

#[cfg(target_arch = "x86_64")]
#[unified_init(INITCALL_DEVICE)]
fn ahci_driver_init() -> Result<(), SystemError> {
    pci_driver_manager().register(Arc::new(AhciPciDriver::new()))
}
