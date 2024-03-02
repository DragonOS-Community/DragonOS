use core::intrinsics::unlikely;

use alloc::{string::ToString, sync::Arc};
use intertrait::CastFrom;
use system_error::SystemError;

use crate::{
    arch::{
        driver::apic::{
            apic_timer::{local_apic_timer_irq_desc_init, APIC_TIMER_IRQ_NUM},
            ioapic::ioapic_init,
        },
        interrupt::{
            entry::arch_setup_interrupt_gate,
            ipi::{arch_ipi_handler_init, send_ipi, IPI_NUM_FLUSH_TLB, IPI_NUM_KICK_CPU},
            msi::{X86MsiAddrHi, X86MsiAddrLoNormal, X86MsiDataNormal, X86_MSI_BASE_ADDRESS_LOW},
        },
    },
    driver::open_firmware::device_node::DeviceNode,
    exception::{
        ipi::{IpiKind, IpiTarget},
        irqchip::{IrqChip, IrqChipData, IrqChipFlags},
        irqdata::IrqData,
        irqdomain::{irq_domain_manager, IrqDomain, IrqDomainBusToken, IrqDomainOps},
        msi::MsiMsg,
        HardwareIrqNumber, IrqNumber,
    },
    kwarn,
    libs::spinlock::{SpinLock, SpinLockGuard},
    smp::{core::smp_get_processor_id, cpu::ProcessorId},
};

use super::{hw_irq::HardwareIrqConfig, CurrentApic, LocalAPIC};

static mut LOCAL_APIC_CHIP: Option<Arc<LocalApicChip>> = None;

pub fn local_apic_chip() -> &'static Arc<LocalApicChip> {
    unsafe { LOCAL_APIC_CHIP.as_ref().unwrap() }
}

#[derive(Debug)]
pub struct LocalApicChip {
    inner: SpinLock<InnerIrqChip>,
}

impl LocalApicChip {
    pub fn new() -> Self {
        Self {
            inner: SpinLock::new(InnerIrqChip {
                flags: IrqChipFlags::empty(),
            }),
        }
    }
}

impl IrqChip for LocalApicChip {
    fn name(&self) -> &'static str {
        "APIC"
    }

    fn can_set_flow_type(&self) -> bool {
        false
    }

    fn irq_disable(&self, _irq: &Arc<IrqData>) {}

    fn irq_ack(&self, _irq: &Arc<IrqData>) {
        CurrentApic.send_eoi();
    }

    fn can_set_affinity(&self) -> bool {
        false
    }

    fn can_mask_ack(&self) -> bool {
        false
    }

    fn irq_enable(&self, _irq: &Arc<IrqData>) -> Result<(), SystemError> {
        // 这里临时处理，后续需要修改
        return Ok(());
    }

    fn irq_unmask(&self, _irq: &Arc<IrqData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn irq_compose_msi_msg(&self, irq: &Arc<IrqData>, msg: &mut MsiMsg) {
        let chip_data = irq.chip_info_read_irqsave().chip_data().unwrap();
        let apicd = chip_data.ref_any().downcast_ref::<ApicChipData>().unwrap();
        let cfg = &apicd.inner().hw_irq_cfg;
        irq_msi_compose_msg(cfg, msg, false);
    }

    fn retrigger(&self, irq: &Arc<IrqData>) -> Result<(), SystemError> {
        let chip_data = irq
            .chip_info_read_irqsave()
            .chip_data()
            .ok_or(SystemError::EINVAL)?;
        let apicd = chip_data
            .ref_any()
            .downcast_ref::<ApicChipData>()
            .ok_or(SystemError::EINVAL)?;
        let inner = apicd.inner();

        send_ipi(
            IpiKind::SpecVector(inner.vector),
            IpiTarget::Specified(inner.cpu),
        );

        Ok(())
    }

    fn flags(&self) -> IrqChipFlags {
        self.inner.lock_irqsave().flags
    }
}

#[derive(Debug)]
struct InnerIrqChip {
    flags: IrqChipFlags,
}

#[derive(Debug)]
struct ApicChipData {
    inner: SpinLock<InnerApicChipData>,
}

impl ApicChipData {
    #[allow(dead_code)]
    pub fn new(
        hw_irq_cfg: HardwareIrqConfig,
        irq: IrqNumber,
        vector: HardwareIrqNumber,
        cpu: ProcessorId,
    ) -> Self {
        Self {
            inner: SpinLock::new(InnerApicChipData {
                hw_irq_cfg,
                irq,
                vector,
                prev_vector: None,
                cpu,
                prev_cpu: None,
                status: ApicChipStatus::empty(),
            }),
        }
    }

    pub fn inner(&self) -> SpinLockGuard<InnerApicChipData> {
        self.inner.lock_irqsave()
    }
}

#[allow(dead_code)]
#[derive(Debug)]
struct InnerApicChipData {
    hw_irq_cfg: HardwareIrqConfig,
    irq: IrqNumber,
    vector: HardwareIrqNumber,
    prev_vector: Option<HardwareIrqNumber>,
    cpu: ProcessorId,
    prev_cpu: Option<ProcessorId>,
    status: ApicChipStatus,
}

impl IrqChipData for ApicChipData {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }
}

bitflags! {
    pub struct ApicChipStatus: u32 {
        const MOVE_IN_PROGRESS = 1 << 0;
        const IS_MANAGED = 1 << 1;
        const CAN_RESERVE = 1 << 2;
        const HAS_RESERVED = 1 << 3;
    }
}

pub(super) fn irq_msi_compose_msg(cfg: &HardwareIrqConfig, msg: &mut MsiMsg, dmar: bool) {
    *msg = MsiMsg::new_zeroed();

    let arch_data = X86MsiDataNormal::new()
        .with_delivery_mode(x86::apic::DeliveryMode::Fixed as u8)
        .with_vector((cfg.vector.data() & 0xff) as u8);
    let mut address_lo = X86MsiAddrLoNormal::new()
        .with_base_address(X86_MSI_BASE_ADDRESS_LOW)
        .with_dest_mode_logical(false)
        .with_destid_0_7(cfg.apic_id.data() & 0xff);

    let mut address_hi = X86MsiAddrHi::new();

    /*
     * 只有IOMMU本身可以使用将目标APIC ID放入地址的高位的技术。
     * 任何其他尝试这样做的东西都只是在写内存，并且需要IR来
     * 寻址不能在正常的32位地址范围内0xFFExxxxx寻址的APIC。
     * 这通常是8位，但一些虚拟化程序允许在位5-11使用扩展的目的地ID字段，
     * 总共支持15位的APIC ID。
     */
    if dmar {
        address_hi.set_destid_8_31(cfg.apic_id.data() >> 8);
    } else if cfg.apic_id.data() < 0x8000 {
        // todo: 判断vmx是否支持 extended destination mode
        // 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/kernel/apic/apic.c?fi=__irq_msi_compose_msg#2580
        address_lo.set_virt_destid_8_14(cfg.apic_id.data() >> 8);
    } else {
        if unlikely(cfg.apic_id.data() > 0xff) {
            kwarn!(
                "irq_msi_compose_msg: Invalid APIC ID: {}",
                cfg.apic_id.data()
            );
        }
    }
    msg.address_hi = address_hi.into();
    msg.address_lo = address_lo.into();
    msg.data = arch_data.into();
}

static mut X86_VECTOR_DOMAIN: Option<Arc<IrqDomain>> = None;

#[inline(always)]
#[allow(dead_code)]
pub fn x86_vector_domain() -> &'static Arc<IrqDomain> {
    unsafe { X86_VECTOR_DOMAIN.as_ref().unwrap() }
}

#[inline(never)]
pub fn arch_early_irq_init() -> Result<(), SystemError> {
    let vec_domain = irq_domain_manager()
        .create_and_add(
            "VECTOR".to_string(),
            &X86VectorDomainOps,
            IrqNumber::new(32),
            HardwareIrqNumber::new(32),
            223,
        )
        .ok_or(SystemError::ENOMEM)?;
    irq_domain_manager().set_default_domain(vec_domain.clone());
    unsafe { X86_VECTOR_DOMAIN = Some(vec_domain) };

    let apic_chip = Arc::new(LocalApicChip::new());

    unsafe { LOCAL_APIC_CHIP = Some(apic_chip) };

    // todo: add vector matrix
    // 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/kernel/apic/vector.c#803
    kwarn!("arch_early_irq_init: todo: add vector matrix");

    local_apic_timer_irq_desc_init();
    arch_ipi_handler_init();
    CurrentApic.init_current_cpu();
    if smp_get_processor_id().data() == 0 {
        unsafe { arch_setup_interrupt_gate() };
        ioapic_init(&[APIC_TIMER_IRQ_NUM, IPI_NUM_KICK_CPU, IPI_NUM_FLUSH_TLB]);
    }
    return Ok(());
}

/// x86的中断域操作
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/kernel/apic/vector.c#693
#[derive(Debug)]
struct X86VectorDomainOps;

impl IrqDomainOps for X86VectorDomainOps {
    fn match_node(
        &self,
        _irq_domain: &Arc<IrqDomain>,
        _device_node: &Arc<DeviceNode>,
        _bus_token: IrqDomainBusToken,
    ) -> bool {
        todo!()
    }

    fn map(
        &self,
        _irq_domain: &Arc<IrqDomain>,
        _hwirq: HardwareIrqNumber,
        _virq: IrqNumber,
    ) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn unmap(&self, _irq_domain: &Arc<IrqDomain>, _virq: IrqNumber) {
        todo!()
    }
}
