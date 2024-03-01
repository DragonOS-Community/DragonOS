use core::{any::Any, fmt::Debug};

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    libs::{casting::DowncastArc, cpumask::CpuMask, spinlock::SpinLock},
    mm::VirtAddr,
};

use super::{
    irqdata::{IrqData, IrqLineStatus},
    irqdomain::IrqDomain,
    manage::IrqManager,
    msi::MsiMsg,
};

/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/irq.h#506
pub trait IrqChip: Sync + Send + Any + Debug {
    fn name(&self) -> &'static str;
    /// start up the interrupt (defaults to ->enable if ENOSYS)
    fn irq_startup(&self, _irq: &Arc<IrqData>) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// shut down the interrupt (defaults to ->disable if ENOSYS)
    fn irq_shutdown(&self, _irq: &Arc<IrqData>) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// enable the interrupt
    ///
    /// (defaults to ->unmask if ENOSYS)
    fn irq_enable(&self, _irq: &Arc<IrqData>) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// disable the interrupt
    fn irq_disable(&self, irq: &Arc<IrqData>);

    /// start of a new interrupt
    fn irq_ack(&self, irq: &Arc<IrqData>);

    /// mask an interrupt source
    ///
    /// 用于屏蔽中断
    ///
    /// 如果返回ENOSYS，则表明irq_mask()不支持.
    ///
    /// 如果返回错误，那么中断的屏蔽状态将不会改变。
    fn irq_mask(&self, _irq: &Arc<IrqData>) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// 指示当前芯片是否实现了`irq_mask_ack`函数
    fn can_mask_ack(&self) -> bool;

    /// ack and mask an interrupt source
    fn irq_mask_ack(&self, _irq: &Arc<IrqData>) {}

    /// unmask an interrupt source
    ///
    /// 用于取消屏蔽中断
    ///
    /// 如果返回ENOSYS，则表明irq_unmask()不支持.
    fn irq_unmask(&self, _irq: &Arc<IrqData>) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }
    /// end of interrupt
    fn irq_eoi(&self, _irq: &Arc<IrqData>) {}

    /// 指示当前芯片是否可以设置中断亲和性。
    fn can_set_affinity(&self) -> bool;

    /// 在SMP机器上设置CPU亲和性。
    ///
    /// 如果force参数为真，它告诉驱动程序无条件地应用亲和性设置。
    /// 不需要对提供的亲和性掩码进行完整性检查。这用于CPU热插拔，其中目标CPU尚未在cpu_online_mask中设置。
    fn irq_set_affinity(
        &self,
        _irq: &Arc<IrqData>,
        _cpu: &CpuMask,
        _force: bool,
    ) -> Result<IrqChipSetMaskResult, SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// retrigger an IRQ to the CPU
    fn retrigger(&self, _irq: &Arc<IrqData>) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// 指示当前芯片是否可以设置中断流类型。
    ///
    /// 如果返回true，则可以调用irq_set_type()。
    fn can_set_flow_type(&self) -> bool;

    /// set the flow type of an interrupt
    ///
    /// flow_type: the flow type to set
    ///
    fn irq_set_type(
        &self,
        _irq: &Arc<IrqData>,
        _flow_type: IrqLineStatus,
    ) -> Result<IrqChipSetMaskResult, SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// enable/disable power management wake-on of an interrupt
    fn irq_set_wake(&self, _irq: &Arc<IrqData>, _on: bool) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// function to lock access to slow bus (i2c) chips
    fn irq_bus_lock(&self, _irq: &Arc<IrqData>) -> Result<(), SystemError> {
        Ok(())
    }

    /// function to sync and unlock slow bus (i2c) chips
    fn irq_bus_sync_unlock(&self, _irq: &Arc<IrqData>) -> Result<(), SystemError> {
        Ok(())
    }

    /// function called from core code on suspend once per
    /// chip, when one or more interrupts are installed
    fn irq_suspend(&self, _irq: &Arc<IrqData>) {}

    /// function called from core code on resume once per chip,
    /// when one ore more interrupts are installed
    fn irq_resume(&self, _irq: &Arc<IrqData>) {}

    /// function called from core code on shutdown once per chip
    fn irq_pm_shutdown(&self, _irq: &Arc<IrqData>) {}

    /// Optional function to set irq_data.mask for special cases
    fn irq_calc_mask(&self, _irq: &Arc<IrqData>) {}

    // todo: print chip

    /// optional to request resources before calling
    /// any other callback related to this irq
    fn irq_request_resources(&self, _irq: &Arc<IrqData>) -> Result<(), SystemError> {
        Ok(())
    }

    /// optional to release resources acquired with
    /// irq_request_resources
    fn irq_release_resources(&self, _irq: &Arc<IrqData>) {}

    /// optional to compose message content for MSI
    ///
    /// 组装MSI消息并返回到msg中
    fn irq_compose_msi_msg(&self, _irq: &Arc<IrqData>, _msg: &mut MsiMsg) {}

    /// optional to write message content for MSI
    fn irq_write_msi_msg(&self, _irq: &Arc<IrqData>, _msg: &MsiMsg) {}

    /// return the internal state of an interrupt
    fn irqchip_state(
        &self,
        _irq: &Arc<IrqData>,
        _which: IrqChipState,
    ) -> Result<bool, SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// set the internal state of an interrupt
    fn set_irqchip_state(
        &self,
        _irq: &Arc<IrqData>,
        _which: IrqChipState,
        _state: bool,
    ) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    // todo: set vcpu affinity

    /// send a single IPI to destination cpus
    fn send_single_ipi(&self, _irq: &Arc<IrqData>, _cpu: u32) {}

    // todo: send ipi with cpu mask

    /// function called from core code before enabling an NMI
    fn irq_nmi_setup(&self, _irq: &Arc<IrqData>) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// function called from core code after disabling an NMI
    fn irq_nmi_teardown(&self, _irq: &Arc<IrqData>) {}

    fn flags(&self) -> IrqChipFlags;
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum IrqChipState {
    /// Is the interrupt pending?
    Pending,
    /// Is the interrupt in progress?
    Active,
    /// Is the interrupt masked?
    Masked,
    /// Is Irq line high?
    LineLevel,
}

/// 中断芯片的数据（per-irq的）
pub trait IrqChipData: Sync + Send + Any + Debug {
    fn as_any_ref(&self) -> &dyn Any;
}

bitflags! {
    /// 定义 IrqGcFlags 位标志
    pub struct IrqGcFlags: u32 {
        /// 通过读取mask reg来初始化mask_cache
        const IRQ_GC_INIT_MASK_CACHE = 1 << 0;
        /// 对于需要在父irq上调用irq_set_wake()的irq芯片, 将irqs的锁类设置为嵌套。Usually GPIO implementations
        const IRQ_GC_INIT_NESTED_LOCK = 1 << 1;
        /// Mask cache是芯片类型私有的
        const IRQ_GC_MASK_CACHE_PER_TYPE = 1 << 2;
        /// 不计算irqData->mask
        const IRQ_GC_NO_MASK = 1 << 3;
        /// 使用大端字节序的寄存器访问（默认：小端LE）
        const IRQ_GC_BE_IO = 1 << 4;
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct IrqChipGeneric {
    inner: SpinLock<InnerIrqChipGeneric>,
}

#[allow(dead_code)]
#[derive(Debug)]
struct InnerIrqChipGeneric {
    /// Register base address
    reg_base: VirtAddr,
    ops: &'static dyn IrqChipGenericOps,
    /// Interrupt base num for this chip
    irq_base: u32,
    /// Number of interrupts handled by this chip
    irq_cnt: u32,
    /// Cached mask register shared between all chip types
    mask_cache: u32,
    /// Cached type register
    type_cache: u32,
    /// Cached polarity register
    polarity_cache: u32,
    /// Interrupt can wakeup from suspend
    wake_enabled: bool,
    /// Interrupt is marked as an wakeup from suspend source
    wake_active: bool,
    /// Number of available irq_chip_type instances (usually 1)
    num_chip_type: u32,
    private_data: Option<Arc<dyn IrqChipGenericPrivateData>>,
    installed: u64,
    unused: u64,
    domain: Weak<IrqDomain>,
    chip_types: Vec<IrqChipType>,
}

pub trait IrqChipGenericOps: Debug + Send + Sync {
    /// Alternate I/O accessor (defaults to readl if NULL)
    unsafe fn reg_readl(&self, addr: VirtAddr) -> u32;

    /// Alternate I/O accessor (defaults to writel if NULL)
    unsafe fn reg_writel(&self, addr: VirtAddr, val: u32);

    /// Function called from core code on suspend once per
    /// chip; can be useful instead of irq_chip::suspend to
    /// handle chip details even when no interrupts are in use
    fn suspend(&self, gc: &Arc<IrqChipGeneric>);
    /// Function called from core code on resume once per chip;
    /// can be useful instead of irq_chip::resume to handle chip
    /// details even when no interrupts are in use
    fn resume(&self, gc: &Arc<IrqChipGeneric>);
}

pub trait IrqChipGenericPrivateData: Sync + Send + Any + Debug {}

#[derive(Debug)]
pub struct IrqChipType {
    // todo https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/irq.h#1024
}

#[derive(Debug)]
pub enum IrqChipSetMaskResult {
    /// core updates mask ok.
    SetMaskOk,
    /// core updates mask ok. No change.
    SetMaskOkNoChange,
    /// core updates mask ok. Done.(same as SetMaskOk)
    ///
    /// 支持堆叠irq芯片的特殊代码, 表示跳过所有子irq芯片。
    SetMaskOkDone,
}

bitflags! {
    /// IrqChip specific flags
    pub struct IrqChipFlags: u32 {
        /// 在调用chip.irq_set_type()之前屏蔽中断
        const IRQCHIP_SET_TYPE_MASKED = 1 << 0;
        /// 只有在irq被处理时才发出irq_eoi()
        const IRQCHIP_EOI_IF_HANDLED = 1 << 1;
        /// 在挂起路径中屏蔽非唤醒irq
        const IRQCHIP_MASK_ON_SUSPEND = 1 << 2;
        /// 只有在irq启用时才调用irq_on/off_line回调
        const IRQCHIP_ONOFFLINE_ENABLED = 1 << 3;
        /// 跳过chip.irq_set_wake()，对于这个irq芯片
        const IRQCHIP_SKIP_SET_WAKE = 1 << 4;
        /// 单次触发不需要屏蔽/取消屏蔽
        const IRQCHIP_ONESHOT_SAFE = 1 << 5;
        /// 芯片在线程模式下需要在取消屏蔽时eoi()
        const IRQCHIP_EOI_THREADED = 1 << 6;
        /// 芯片可以为Level MSIs提供两个门铃
        const IRQCHIP_SUPPORTS_LEVEL_MSI = 1 << 7;
        /// 芯片可以传递NMIs，仅适用于根irqchips
        const IRQCHIP_SUPPORTS_NMI = 1 << 8;
        /// 在挂起路径中，如果它们处于禁用状态，则调用__enable_irq()/__disable_irq()以唤醒irq
        const IRQCHIP_ENABLE_WAKEUP_ON_SUSPEND = 1 << 9;
        /// 在启动前更新默认亲和性
        const IRQCHIP_AFFINITY_PRE_STARTUP = 1 << 10;
        /// 不要在这个芯片中改变任何东西
        const IRQCHIP_IMMUTABLE = 1 << 11;
    }
}

impl IrqManager {
    /// Acknowledge the parent interrupt
    pub fn irq_chip_ack_parent(&self, irq_data: &Arc<IrqData>) {
        let parent_data = irq_data.parent_data().map(|p| p.upgrade()).flatten();

        if let Some(parent_data) = parent_data {
            let parent_chip = parent_data.chip();
            parent_chip.irq_ack(&parent_data);
        }
    }

    /// 在硬件中重新触发中断
    ///
    /// 遍历中断域的层次结构，并检查是否存在一个硬件重新触发函数。如果存在则调用它
    pub fn irq_chip_retrigger_hierarchy(&self, irq_data: &Arc<IrqData>) -> Result<(), SystemError> {
        let mut data: Option<Arc<IrqData>> = Some(irq_data.clone());
        loop {
            if let Some(d) = data {
                if let Err(e) = d.chip().retrigger(&d) {
                    if e == SystemError::ENOSYS {
                        data = d.parent_data().map(|p| p.upgrade()).flatten();
                    } else {
                        return Err(e);
                    }
                } else {
                    return Ok(());
                }
            } else {
                break;
            }
        }

        return Ok(());
    }
}
