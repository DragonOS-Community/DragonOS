//! Runtime receive-filter projection for virtio-net.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicBool, Ordering};

use smoltcp::iface::Interface;
use system_error::SystemError;
use virtio_drivers::device::net::RxFilterCommand;

use crate::{
    driver::{
        net::{types::InterfaceFlags, PacketMacMembership},
        virtio::virtio_drivers_error_to_system_error,
    },
    exception::workqueue::{schedule_work, Work},
    libs::{mutex::Mutex, spinlock::SpinLock},
    time::{
        sleep::{busy_wait_us, nanosleep},
        Instant, PosixTimeSpec,
    },
};

use super::VirtIONicDeviceInner;

// A bounded projection keeps the control path allocation-free. If the
// subscriptions do not fit, widen reception for that address class.
const MAX_FILTER_ENTRIES: usize = 64;
const COMMAND_TIMEOUT_US: i64 = 100_000;
const COMMAND_POLL_NS: i64 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MacEntries {
    addresses: [[u8; 6]; MAX_FILTER_ENTRIES],
    count: usize,
    overflow: bool,
}

impl MacEntries {
    const fn new() -> Self {
        Self {
            addresses: [[0; 6]; MAX_FILTER_ENTRIES],
            count: 0,
            overflow: false,
        }
    }

    fn insert(&mut self, address: [u8; 6]) {
        if self.overflow || self.addresses().contains(&address) {
            return;
        }
        if let Some(entry) = self.addresses.get_mut(self.count) {
            *entry = address;
            self.count += 1;
        } else {
            self.overflow = true;
        }
    }

    fn addresses(&self) -> &[[u8; 6]] {
        &self.addresses[..self.count]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MacTable {
    unicast: MacEntries,
    multicast: MacEntries,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RxMode {
    promiscuous: Option<bool>,
    all_multicast: Option<bool>,
    mac_table: Option<MacTable>,
}

impl RxMode {
    fn capture(
        interface: &Interface,
        flags: InterfaceFlags,
        packet_macs: &[(PacketMacMembership, u32)],
    ) -> Self {
        let mut multicast = MacEntries::new();
        let mut unicast = MacEntries::new();
        interface.for_each_multicast_ethernet_address(|address| multicast.insert(address.0));
        for (member, _) in packet_macs {
            let table = if member.multicast {
                &mut multicast
            } else {
                &mut unicast
            };
            table.insert(member.address.0);
        }
        Self {
            promiscuous: Some(flags.contains(InterfaceFlags::PROMISC) || unicast.overflow),
            all_multicast: Some(flags.contains(InterfaceFlags::ALLMULTI) || multicast.overflow),
            mac_table: Some(MacTable { unicast, multicast }),
        }
    }
}

#[derive(Debug)]
pub(super) struct RxFilter {
    device: VirtIONicDeviceInner,
    enabled: AtomicBool,
    desired: SpinLock<RxMode>,
    applied: Mutex<RxMode>,
}

impl RxFilter {
    pub(super) fn new(device: VirtIONicDeviceInner) -> Result<Arc<Self>, SystemError> {
        Arc::try_new(Self {
            device,
            enabled: AtomicBool::new(false),
            desired: SpinLock::new(RxMode::default()),
            applied: Mutex::new(RxMode::default()),
        })
        .map_err(|_| SystemError::ENOMEM)
    }

    pub(super) fn projector(
        self: &Arc<Self>,
    ) -> Result<Box<super::super::iface_common::ReceiveFilter>, SystemError> {
        let weak = Arc::downgrade(self);
        let work = Work::try_new(move || {
            if let Some(filter) = weak.upgrade() {
                filter.sync();
            }
        })?;
        let filter = self.clone();
        let projector = Box::try_new(move |interface: &Interface, flags, packet_macs: &[_]| {
            let next = RxMode::capture(interface, flags, packet_macs);
            let mut desired = filter.desired.lock_irqsave();
            if *desired == next {
                return;
            }
            *desired = next;
            drop(desired);
            if filter.enabled.load(Ordering::Acquire) {
                schedule_work(work.clone());
            }
        })
        .map_err(|_| SystemError::ENOMEM)?;
        Ok(projector)
    }

    /// Called after link configuration drops protocol locks, before autoconf
    /// starts sending on a newly enabled interface.
    pub(super) fn synchronize_link(&self, up: bool) {
        if up {
            self.enabled.store(true, Ordering::Release);
        }
        if self.enabled.load(Ordering::Acquire) {
            self.sync();
        }
    }

    fn sync(&self) {
        if let Err(error) = self.synchronize(&mut self.applied.lock()) {
            log::warn!("virtio-net could not update receive filter: {error:?}");
        }
    }

    pub(super) fn set_bridge_mode(
        &self,
        common: &super::super::IfaceCommon,
        enable: bool,
    ) -> Result<(), SystemError> {
        let mut applied = self.applied.lock();
        if enable {
            // Start from an acknowledged standalone mode even when a DOWN
            // interface has never applied its initial projection.
            self.synchronize(&mut applied)?;
        }
        common.set_bridge_receive_mode(enable)?;
        self.enabled.store(true, Ordering::Release);
        if !enable {
            // Software detach has already committed. Retain any successful
            // narrowing even if a later filter operation is declined.
            let desired = *self.desired.lock_irqsave();
            return self.program(&mut applied, desired);
        }
        if let Err(error) = self.synchronize(&mut applied) {
            // synchronize has already rolled back the acknowledged changes.
            common
                .set_bridge_receive_mode(false)
                .expect("removing a receive-mode reference cannot overflow");
            return Err(error);
        }
        Ok(())
    }

    fn synchronize(&self, applied: &mut RxMode) -> Result<(), SystemError> {
        let desired = *self.desired.lock_irqsave();
        let before = *applied;
        if let Err(error) = self.program(applied, desired) {
            // Roll back only settings changed before an explicit refusal.
            // A transport fault has already stopped the queues.
            let poisoned = self.device.inner.lock_irqsave().poisoned;
            if *applied != before && !poisoned && self.program(applied, before).is_err() {
                self.fail("could not restore receive filter");
            }
            return Err(error);
        }
        Ok(())
    }

    fn fail(&self, reason: &'static str) {
        self.device.inner.lock_irqsave().poison(reason);
        self.mark_no_carrier();
    }

    fn mark_no_carrier(&self) {
        use super::{napi_schedule, NetDeivceState, Operstate};
        if let Some(iface) = self.device.iface() {
            iface.set_operstate(Operstate::IF_OPER_DOWN);
            iface.set_net_state(NetDeivceState::__LINK_STATE_NOCARRIER);
            if let Some(napi) = iface.napi_struct() {
                napi_schedule(napi);
            }
        }
    }

    fn program(&self, applied: &mut RxMode, target: RxMode) -> Result<(), SystemError> {
        // Fill the table before narrowing reception, including after detach.
        if let Some(table) = target
            .mac_table
            .filter(|_| applied.mac_table != target.mac_table)
        {
            self.command(RxFilterCommand::MacTable {
                unicast: table.unicast.addresses(),
                multicast: table.multicast.addresses(),
            })?;
            applied.mac_table = Some(table);
        }
        if let Some(enabled) = target
            .all_multicast
            .filter(|_| applied.all_multicast != target.all_multicast)
        {
            self.command(RxFilterCommand::AllMulticast(enabled))?;
            applied.all_multicast = Some(enabled);
        }
        if let Some(enabled) = target
            .promiscuous
            .filter(|_| applied.promiscuous != target.promiscuous)
        {
            self.command(RxFilterCommand::Promiscuous(enabled))?;
            applied.promiscuous = Some(enabled);
        }
        Ok(())
    }

    fn command(&self, command: RxFilterCommand<'_>) -> Result<(), SystemError> {
        {
            let mut driver = self.device.inner.lock_irqsave();
            if driver.poisoned {
                return Err(SystemError::EIO);
            }
            driver
                .inner
                .start_rx_filter(command)
                .map_err(virtio_drivers_error_to_system_error)?;
        }
        let deadline = Instant::now()
            .total_micros()
            .saturating_add(COMMAND_TIMEOUT_US);
        let mut can_sleep = true;
        loop {
            let completed = {
                let mut driver = self.device.inner.lock_irqsave();
                match driver.inner.poll_rx_filter() {
                    Ok(completed) => completed,
                    Err(error) => {
                        driver.poison("invalid receive-filter completion");
                        drop(driver);
                        self.mark_no_carrier();
                        return Err(virtio_drivers_error_to_system_error(error));
                    }
                }
            };
            match completed {
                Some(true) => return Ok(()),
                Some(false) => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
                None => {}
            }
            if Instant::now().total_micros() >= deadline {
                self.fail("receive-filter timeout");
                return Err(SystemError::ETIMEDOUT);
            }
            // A local signal or timer-allocation failure cannot cancel
            // a device-owned descriptor. Poll to the same deadline.
            if can_sleep {
                can_sleep = nanosleep(PosixTimeSpec::new(0, COMMAND_POLL_NS)).is_ok();
            } else {
                busy_wait_us(50);
                crate::sched::sched_yield();
            }
        }
    }
}
