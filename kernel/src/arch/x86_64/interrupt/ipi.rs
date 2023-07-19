use crate::exception::ipi::{IpiKind, IpiTarget};

extern "C" {
    pub fn apic_write_icr(value: u64);
    pub fn apic_x2apic_enabled() -> bool;
}

/// IPI的种类(架构相关，指定了向量号)
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[repr(u8)]
pub enum ArchIpiKind {
    KickCpu = 200,
    FlushTLB = 201,
    StartUp = 0,
}

impl From<IpiKind> for ArchIpiKind {
    fn from(kind: IpiKind) -> Self {
        match kind {
            IpiKind::KickCpu => ArchIpiKind::KickCpu,
            IpiKind::FlushTLB => ArchIpiKind::FlushTLB,
            IpiKind::StartUp => ArchIpiKind::StartUp,
        }
    }
}

/// IPI投递目标
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ArchIpiTarget {
    /// 当前CPU
    Current,
    /// 所有CPU
    All,
    /// 除了当前CPU以外的所有CPU
    Other,
    /// 指定的CPU
    Specified(usize),
}

impl From<IpiTarget> for ArchIpiTarget {
    fn from(target: IpiTarget) -> Self {
        match target {
            IpiTarget::Current => ArchIpiTarget::Current,
            IpiTarget::All => ArchIpiTarget::All,
            IpiTarget::Other => ArchIpiTarget::Other,
            IpiTarget::Specified(cpu_id) => ArchIpiTarget::Specified(cpu_id),
        }
    }
}

impl ArchIpiTarget {
    pub fn shorthand(&self) -> u8 {
        match self {
            ArchIpiTarget::Specified(_) => 0,
            ArchIpiTarget::Current => 1,
            ArchIpiTarget::All => 2,
            ArchIpiTarget::Other => 3,
        }
    }
}

impl Into<x86::apic::DestinationShorthand> for ArchIpiTarget {
    fn into(self) -> x86::apic::DestinationShorthand {
        match self {
            ArchIpiTarget::Specified(_) => x86::apic::DestinationShorthand::NoShorthand,
            ArchIpiTarget::Current => x86::apic::DestinationShorthand::Myself,
            ArchIpiTarget::All => x86::apic::DestinationShorthand::AllIncludingSelf,
            ArchIpiTarget::Other => x86::apic::DestinationShorthand::AllExcludingSelf,
        }
    }
}

impl Into<x86::apic::ApicId> for ArchIpiTarget {
    fn into(self) -> x86::apic::ApicId {
        let id = match self {
            ArchIpiTarget::Specified(cpu_id) => cpu_id,
            _ => 0,
        };

        if unsafe { apic_x2apic_enabled() } {
            return x86::apic::ApicId::X2Apic(id as u32);
        } else {
            return x86::apic::ApicId::XApic(id as u8);
        }
    }
}

#[inline(always)]
pub fn send_ipi(kind: IpiKind, target: IpiTarget) {
    // kdebug!("send_ipi: {:?} {:?}", kind, target);

    let ipi_vec = ArchIpiKind::from(kind) as u8;
    let target = ArchIpiTarget::from(target);
    let shorthand: x86::apic::DestinationShorthand = target.into();
    let destination: x86::apic::ApicId = target.into();
    if unsafe { apic_x2apic_enabled() } {
        // kdebug!("send_ipi: x2apic");
        let icr = x86::apic::Icr::for_x2apic(
            ipi_vec,
            destination,
            shorthand,
            x86::apic::DeliveryMode::Fixed,
            x86::apic::DestinationMode::Physical,
            x86::apic::DeliveryStatus::Idle,
            x86::apic::Level::Assert,
            x86::apic::TriggerMode::Edge,
        );

        unsafe {
            apic_write_icr(((icr.upper() as u64) << 32) | icr.lower() as u64);
        }
    } else {
        // kdebug!("send_ipi: xapic");
        let icr = x86::apic::Icr::for_xapic(
            ipi_vec,
            destination,
            shorthand,
            x86::apic::DeliveryMode::Fixed,
            x86::apic::DestinationMode::Physical,
            x86::apic::DeliveryStatus::Idle,
            x86::apic::Level::Assert,
            x86::apic::TriggerMode::Edge,
        );

        unsafe {
            apic_write_icr(((icr.upper() as u64) << 32) | icr.lower() as u64);
        }
    }
}
