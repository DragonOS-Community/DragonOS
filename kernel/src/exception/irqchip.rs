use core::{any::Any, fmt::Debug, intrinsics::unlikely};

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    exception::{
        dummychip::no_irq_chip,
        handle::{bad_irq_handler, mask_ack_irq},
        irqdata::IrqStatus,
        irqdesc::irq_desc_manager,
        manage::irq_manager,
    },
    libs::{
        cpumask::CpuMask,
        once::Once,
        spinlock::{SpinLock, SpinLockGuard},
    },
    mm::VirtAddr,
    smp::cpu::ProcessorId,
};

use super::{
    irqdata::{IrqData, IrqHandlerData, IrqLineStatus},
    irqdesc::{InnerIrqDesc, IrqAction, IrqDesc, IrqFlowHandler, IrqHandler, IrqReturn},
    irqdomain::IrqDomain,
    manage::IrqManager,
    msi::MsiMsg,
    IrqNumber,
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
    /// 如果返回ENOSYS，则表明irq_mask()不支持. 那么中断机制代码将调用irq_disable()。
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

#[allow(dead_code)]
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
    #[allow(dead_code)]
    pub fn irq_chip_ack_parent(&self, irq_data: &Arc<IrqData>) {
        let parent_data = irq_data.parent_data().map(|p| p.upgrade()).flatten();

        if let Some(parent_data) = parent_data {
            let parent_chip = parent_data.chip_info_read_irqsave().chip();
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
                if let Err(e) = d.chip_info_read_irqsave().chip().retrigger(&d) {
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

    pub(super) fn __irq_set_handler(
        &self,
        irq: IrqNumber,
        handler: &'static dyn IrqFlowHandler,
        is_chained: bool,
        name: Option<String>,
    ) {
        let r = irq_desc_manager().lookup_and_lock_bus(irq, false, false);
        if r.is_none() {
            return;
        }

        let irq_desc = r.unwrap();

        let mut desc_inner = irq_desc.inner();
        self.__irq_do_set_handler(&irq_desc, &mut desc_inner, Some(handler), is_chained, name);

        drop(desc_inner);
        irq_desc.chip_bus_sync_unlock();
    }

    fn __irq_do_set_handler(
        &self,
        desc: &Arc<IrqDesc>,
        desc_inner: &mut SpinLockGuard<'_, InnerIrqDesc>,
        mut handler: Option<&'static dyn IrqFlowHandler>,
        is_chained: bool,
        name: Option<String>,
    ) {
        if handler.is_none() {
            handler = Some(bad_irq_handler());
        } else {
            let mut irq_data = Some(desc_inner.irq_data().clone());

            /*
             * 在具有中断域继承的domain中，我们可能会遇到这样的情况，
             * 最外层的芯片还没有设置好，但是内部的芯片已经存在了。
             * 我们选择安装处理程序，而不是退出，
             * 但显然我们此时无法启用/启动中断。
             */
            while irq_data.is_some() {
                let dt = irq_data.as_ref().unwrap().clone();

                let chip_info = dt.chip_info_read_irqsave();

                if !Arc::ptr_eq(&chip_info.chip(), &no_irq_chip()) {
                    break;
                }

                /*
                 * 如果最外层的芯片没有设置好，并且预期立即开始中断，
                 * 则放弃。
                 */
                if unlikely(is_chained) {
                    kwarn!(
                        "Chained handler for irq {} is not supported",
                        dt.irq().data()
                    );
                    return;
                }

                //  try the parent
                let parent_data = dt.parent_data().map(|p| p.upgrade()).flatten();

                irq_data = parent_data;
            }

            if unlikely(
                irq_data.is_none()
                    || Arc::ptr_eq(
                        &irq_data.as_ref().unwrap().chip_info_read_irqsave().chip(),
                        &no_irq_chip(),
                    ),
            ) {
                kwarn!("No irq chip for irq {}", desc_inner.irq_data().irq().data());
                return;
            }
        }
        let handler = handler.unwrap();
        if core::ptr::eq(handler, bad_irq_handler()) {
            if Arc::ptr_eq(
                &desc_inner.irq_data().chip_info_read_irqsave().chip(),
                &no_irq_chip(),
            ) {
                let irq_data = desc_inner.irq_data();
                mask_ack_irq(irq_data);

                irq_data.irqd_set(IrqStatus::IRQD_IRQ_DISABLED);

                if is_chained {
                    desc_inner.clear_actions();
                }
                desc_inner.set_depth(1);
            }
        }
        let chip = desc_inner.irq_data().chip_info_read_irqsave().chip();
        desc.set_handler_no_lock_inner(handler, desc_inner.irq_data(), &chip);
        desc_inner.set_name(name);

        if !core::ptr::eq(handler, bad_irq_handler()) && is_chained {
            let trigger_type = desc_inner.common_data().trigger_type();

            /*
             * 我们即将立即启动这个中断，
             * 因此需要设置触发配置。
             * 但是 .irq_set_type 回调可能已经覆盖了
             * irqflowhandler，忽略了我们正在处理的
             * 是一个链式中断。立即重置它，因为我们
             * 确实知道更好的处理方式。
             */

            if trigger_type != IrqLineStatus::IRQ_TYPE_NONE {
                irq_manager()
                    .do_set_irq_trigger(desc.clone(), desc_inner, trigger_type)
                    .ok();
                desc.set_handler(handler);
            }

            desc_inner.set_noprobe();
            desc_inner.set_norequest();
            desc_inner.set_nothread();

            desc_inner.clear_actions();
            desc_inner.add_action(chained_action());

            irq_manager()
                .irq_activate_and_startup(desc, desc_inner, IrqManager::IRQ_RESEND)
                .ok();
        }

        return;
    }

    pub fn irq_set_handler_data(
        &self,
        irq: IrqNumber,
        data: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<(), SystemError> {
        let desc = irq_desc_manager().lookup(irq).ok_or(SystemError::EINVAL)?;
        desc.inner().common_data().inner().set_handler_data(data);

        return Ok(());
    }

    pub fn irq_percpu_disable(
        &self,
        desc: &Arc<IrqDesc>,
        irq_data: &Arc<IrqData>,
        irq_chip: &Arc<dyn IrqChip>,
        cpu: ProcessorId,
    ) {
        if let Err(e) = irq_chip.irq_mask(irq_data) {
            if e == SystemError::ENOSYS {
                irq_chip.irq_disable(irq_data);
            }
        }

        desc.inner()
            .percpu_enabled_mut()
            .as_mut()
            .unwrap()
            .set(cpu, false);
    }
}

lazy_static! {
    pub(super) static ref CHAINED_ACTION: Arc<IrqAction> = IrqAction::new(
        IrqNumber::new(0),
        "".to_string(),
        Some(&ChainedActionHandler),
        None,
    );
}

#[allow(dead_code)]
pub(super) fn chained_action() -> Arc<IrqAction> {
    CHAINED_ACTION.clone()
}

/// Chained handlers 永远不应该在它们的IRQ上调用irqaction。如果发生这种情况，
/// 这个默认irqaction将发出警告。
#[derive(Debug)]
struct ChainedActionHandler;

impl IrqHandler for ChainedActionHandler {
    fn handle(
        &self,
        irq: IrqNumber,
        _static_data: Option<&dyn IrqHandlerData>,
        _dynamic_data: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<IrqReturn, SystemError> {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            kwarn!("Chained irq {} should not call an action.", irq.data());
        });

        Ok(IrqReturn::NotHandled)
    }
}
