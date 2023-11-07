use x86::apic::ApicId;

use crate::{
    arch::{
        driver::apic::{CurrentApic, LocalAPIC},
        smp::SMP_BOOT_DATA,
    },
    exception::ipi::{IpiKind, IpiTarget},
    syscall::SystemError,
};

/// IPI的种类(架构相关，指定了向量号)
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[repr(u8)]
pub enum ArchIpiKind {
    KickCpu = 200,
    FlushTLB = 201,
}

impl From<IpiKind> for ArchIpiKind {
    fn from(kind: IpiKind) -> Self {
        match kind {
            IpiKind::KickCpu => ArchIpiKind::KickCpu,
            IpiKind::FlushTLB => ArchIpiKind::FlushTLB,
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
    Specified(x86::apic::ApicId),
}

impl From<IpiTarget> for ArchIpiTarget {
    fn from(target: IpiTarget) -> Self {
        match target {
            IpiTarget::Current => ArchIpiTarget::Current,
            IpiTarget::All => ArchIpiTarget::All,
            IpiTarget::Other => ArchIpiTarget::Other,
            IpiTarget::Specified(cpu_id) => {
                ArchIpiTarget::Specified(Self::cpu_id_to_apic_id(cpu_id as u32))
            }
        }
    }
}

impl Into<ApicId> for ArchIpiTarget {
    fn into(self) -> ApicId {
        if let ArchIpiTarget::Specified(id) = self {
            return id;
        } else {
            if CurrentApic.x2apic_enabled() {
                return x86::apic::ApicId::X2Apic(0);
            } else {
                return x86::apic::ApicId::XApic(0);
            }
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

    #[inline(always)]
    fn cpu_id_to_apic_id(cpu_id: u32) -> x86::apic::ApicId {
        if CurrentApic.x2apic_enabled() {
            x86::apic::ApicId::X2Apic(cpu_id as u32)
        } else {
            x86::apic::ApicId::XApic(cpu_id as u8)
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

#[inline(always)]
pub fn send_ipi(kind: IpiKind, target: IpiTarget) {
    // kdebug!("send_ipi: {:?} {:?}", kind, target);

    let ipi_vec = ArchIpiKind::from(kind) as u8;
    let target = ArchIpiTarget::from(target);
    let shorthand: x86::apic::DestinationShorthand = target.into();
    let destination: x86::apic::ApicId = target.into();
    let icr = if CurrentApic.x2apic_enabled() {
        // kdebug!("send_ipi: x2apic");
        x86::apic::Icr::for_x2apic(
            ipi_vec,
            destination,
            shorthand,
            x86::apic::DeliveryMode::Fixed,
            x86::apic::DestinationMode::Physical,
            x86::apic::DeliveryStatus::Idle,
            x86::apic::Level::Assert,
            x86::apic::TriggerMode::Edge,
        )
    } else {
        // kdebug!("send_ipi: xapic");
        x86::apic::Icr::for_xapic(
            ipi_vec,
            destination,
            shorthand,
            x86::apic::DeliveryMode::Fixed,
            x86::apic::DestinationMode::Physical,
            x86::apic::DeliveryStatus::Idle,
            x86::apic::Level::Assert,
            x86::apic::TriggerMode::Edge,
        )
    };

    CurrentApic.write_icr(icr);
}

/// 发送smp初始化IPI
pub fn ipi_send_smp_init() -> Result<(), SystemError> {
    let target = ArchIpiTarget::Other;
    let icr = if CurrentApic.x2apic_enabled() {
        x86::apic::Icr::for_x2apic(
            0,
            target.into(),
            x86::apic::DestinationShorthand::AllExcludingSelf,
            x86::apic::DeliveryMode::Init,
            x86::apic::DestinationMode::Physical,
            x86::apic::DeliveryStatus::Idle,
            x86::apic::Level::Deassert,
            x86::apic::TriggerMode::Edge,
        )
    } else {
        x86::apic::Icr::for_xapic(
            0,
            target.into(),
            x86::apic::DestinationShorthand::AllExcludingSelf,
            x86::apic::DeliveryMode::Init,
            x86::apic::DestinationMode::Physical,
            x86::apic::DeliveryStatus::Idle,
            x86::apic::Level::Deassert,
            x86::apic::TriggerMode::Edge,
        )
    };
    CurrentApic.write_icr(icr);
    return Ok(());
}

/// 发送smp启动IPI
///
/// ## 参数
///
/// * `target_cpu` - 目标CPU
pub fn ipi_send_smp_startup(target_cpu: u32) -> Result<(), SystemError> {
    if target_cpu as usize >= SMP_BOOT_DATA.cpu_count() {
        return Err(SystemError::EINVAL);
    }
    let target: ArchIpiTarget = IpiTarget::Specified(target_cpu as usize).into();

    let icr = if CurrentApic.x2apic_enabled() {
        x86::apic::Icr::for_x2apic(
            0x20,
            target.into(),
            x86::apic::DestinationShorthand::NoShorthand,
            x86::apic::DeliveryMode::StartUp,
            x86::apic::DestinationMode::Physical,
            x86::apic::DeliveryStatus::Idle,
            x86::apic::Level::Deassert,
            x86::apic::TriggerMode::Edge,
        )
    } else {
        x86::apic::Icr::for_xapic(
            0x20,
            target.into(),
            x86::apic::DestinationShorthand::NoShorthand,
            x86::apic::DeliveryMode::StartUp,
            x86::apic::DestinationMode::Physical,
            x86::apic::DeliveryStatus::Idle,
            x86::apic::Level::Deassert,
            x86::apic::TriggerMode::Edge,
        )
    };

    CurrentApic.write_icr(icr);
    return Ok(());
}
