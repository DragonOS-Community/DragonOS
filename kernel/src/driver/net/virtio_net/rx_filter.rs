//! Runtime receive-filter projection for virtio-net.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicBool, Ordering};

use smoltcp::{config, iface::Interface};
use system_error::SystemError;
use virtio_drivers::device::net::RxFilterCommand;

use crate::{
    driver::{net::types::InterfaceFlags, virtio::virtio_drivers_error_to_system_error},
    exception::workqueue::{schedule_work, Work},
    libs::{mutex::Mutex, spinlock::SpinLock},
    time::{
        sleep::{busy_wait_us, nanosleep},
        Instant, PosixTimeSpec,
    },
};

use super::VirtIONicDeviceInner;

const MAX_GROUPS: usize =
    config::IFACE_MAX_ADDR_COUNT + config::IFACE_MAX_MULTICAST_GROUP_COUNT + 3;
const COMMAND_TIMEOUT_US: i64 = 100_000;
const COMMAND_POLL_NS: i64 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GroupTable {
    entries: [[u8; 6]; MAX_GROUPS],
    count: usize,
}

impl GroupTable {
    fn capture(interface: &Interface) -> Self {
        let mut table = Self {
            entries: [[0; 6]; MAX_GROUPS],
            count: 0,
        };
        interface.for_each_multicast_ethernet_address(|address| {
            if !table.entries().contains(&address.0) {
                table.entries[table.count] = address.0;
                table.count += 1;
            }
        });
        table
    }

    fn entries(&self) -> &[[u8; 6]] {
        &self.entries[..self.count]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RxMode {
    promiscuous: Option<bool>,
    all_multicast: Option<bool>,
    groups: Option<GroupTable>,
}

impl RxMode {
    fn capture(interface: &Interface, flags: InterfaceFlags) -> Self {
        Self {
            promiscuous: Some(flags.contains(InterfaceFlags::PROMISC)),
            all_multicast: Some(flags.contains(InterfaceFlags::ALLMULTI)),
            groups: Some(GroupTable::capture(interface)),
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
        let projector = Box::try_new(move |interface: &Interface, flags| {
            let next = RxMode::capture(interface, flags);
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
        if let Some(groups) = target.groups.filter(|_| applied.groups != target.groups) {
            self.command(RxFilterCommand::MulticastTable(groups.entries()))?;
            applied.groups = Some(groups);
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
