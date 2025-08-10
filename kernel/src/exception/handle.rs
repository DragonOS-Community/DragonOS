use core::{intrinsics::unlikely, ops::BitAnd};

use alloc::sync::Arc;
use log::{debug, error, warn};
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, CurrentIrqArch},
    exception::{irqchip::IrqChipFlags, irqdesc::InnerIrqDesc},
    libs::{once::Once, spinlock::SpinLockGuard},
    process::{ProcessFlags, ProcessManager},
    smp::core::smp_get_processor_id,
};

use super::{
    irqchip::IrqChip,
    irqdata::{IrqData, IrqHandlerData, IrqStatus},
    irqdesc::{
        InnerIrqAction, IrqDesc, IrqDescState, IrqFlowHandler, IrqReturn, ThreadedHandlerFlags,
    },
    manage::{irq_manager, IrqManager},
    InterruptArch, IrqNumber,
};

/// 获取用于处理错误的中断的处理程序
#[inline(always)]
pub fn bad_irq_handler() -> &'static dyn IrqFlowHandler {
    &HandleBadIrq
}

/// 获取用于处理快速EOI的中断的处理程序
#[inline(always)]
pub fn fast_eoi_irq_handler() -> &'static dyn IrqFlowHandler {
    &FastEOIIrqHandler
}

/// 获取用于处理边沿触发中断的处理程序
#[inline(always)]
#[allow(dead_code)]
pub fn edge_irq_handler() -> &'static dyn IrqFlowHandler {
    &EdgeIrqHandler
}

/// handle spurious and unhandled irqs
#[derive(Debug)]
struct HandleBadIrq;

impl IrqFlowHandler for HandleBadIrq {
    /// 参考: https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/handle.c?fi=handle_bad_irq#33
    fn handle(&self, irq_desc: &Arc<IrqDesc>, _trap_frame: &mut TrapFrame) {
        // todo: print_irq_desc
        // todo: 增加kstat计数
        CurrentIrqArch::ack_bad_irq(irq_desc.irq());
    }
}

#[derive(Debug)]
struct FastEOIIrqHandler;

impl IrqFlowHandler for FastEOIIrqHandler {
    /// https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/chip.c?r=&mo=17578&fi=689#689
    fn handle(&self, irq_desc: &Arc<IrqDesc>, _trap_frame: &mut TrapFrame) {
        let chip = irq_desc.irq_data().chip_info_read_irqsave().chip();

        let mut desc_inner = irq_desc.inner();
        let out = |din: SpinLockGuard<InnerIrqDesc>| {
            if !chip.flags().contains(IrqChipFlags::IRQCHIP_EOI_IF_HANDLED) {
                chip.irq_eoi(din.irq_data());
            }
        };
        if !irq_may_run(&desc_inner) {
            out(desc_inner);
            return;
        }

        desc_inner
            .internal_state_mut()
            .remove(IrqDescState::IRQS_REPLAY | IrqDescState::IRQS_WAITING);

        if desc_inner.actions().is_empty() || desc_inner.common_data().disabled() {
            desc_inner
                .internal_state_mut()
                .insert(IrqDescState::IRQS_PENDING);
            mask_irq(desc_inner.irq_data());
            out(desc_inner);
            return;
        }

        desc_inner = handle_irq_event(irq_desc, desc_inner);
        cond_unmask_eoi_irq(&desc_inner, &chip);

        return;
    }
}

#[derive(Debug)]
struct EdgeIrqHandler;

impl IrqFlowHandler for EdgeIrqHandler {
    // https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/chip.c?fi=handle_edge_irq#775
    fn handle(&self, irq_desc: &Arc<IrqDesc>, _trap_frame: &mut TrapFrame) {
        let mut desc_inner_guard: SpinLockGuard<'_, InnerIrqDesc> = irq_desc.inner();
        if !irq_may_run(&desc_inner_guard) {
            // debug!("!irq_may_run");
            desc_inner_guard
                .internal_state_mut()
                .insert(IrqDescState::IRQS_PENDING);
            mask_ack_irq(desc_inner_guard.irq_data());
            return;
        }

        if desc_inner_guard.common_data().disabled() {
            // debug!("desc_inner_guard.common_data().disabled()");
            desc_inner_guard
                .internal_state_mut()
                .insert(IrqDescState::IRQS_PENDING);
            mask_ack_irq(desc_inner_guard.irq_data());
            return;
        }

        let irq_data = desc_inner_guard.irq_data().clone();

        irq_data.chip_info_read_irqsave().chip().irq_ack(&irq_data);

        loop {
            if unlikely(desc_inner_guard.actions().is_empty()) {
                debug!("no action for irq {}", irq_data.irq().data());
                irq_manager().mask_irq(&irq_data);
                return;
            }

            // 当我们在处理一个中断时，如果另一个中断到来，我们本可以屏蔽它.
            // 如果在此期间没有被禁用，请重新启用它。
            if desc_inner_guard
                .internal_state()
                .contains(IrqDescState::IRQS_PENDING)
            {
                let status = desc_inner_guard.common_data().status();
                if !status.disabled() && status.masked() {
                    // debug!("re-enable irq");
                    irq_manager().unmask_irq(&desc_inner_guard);
                }
            }

            // debug!("handle_irq_event");

            desc_inner_guard = handle_irq_event(irq_desc, desc_inner_guard);

            if !desc_inner_guard
                .internal_state()
                .contains(IrqDescState::IRQS_PENDING)
                || desc_inner_guard.common_data().disabled()
            {
                break;
            }
        }
    }
}

/// 判断中断是否可以运行
fn irq_may_run(desc_inner_guard: &SpinLockGuard<'_, InnerIrqDesc>) -> bool {
    let mask = IrqStatus::IRQD_IRQ_INPROGRESS | IrqStatus::IRQD_WAKEUP_ARMED;
    let status = desc_inner_guard.common_data().status();

    // 如果中断不在处理中并且没有被唤醒，则可以运行
    if status.bitand(mask).is_empty() {
        return true;
    }

    // todo: 检查其他处理器是否在轮询当前中断
    return false;
}

pub(super) fn mask_ack_irq(irq_data: &Arc<IrqData>) {
    let chip = irq_data.chip_info_read_irqsave().chip();
    if chip.can_mask_ack() {
        chip.irq_mask_ack(irq_data);
        irq_data.common_data().set_masked();
    } else {
        irq_manager().mask_irq(irq_data);
        chip.irq_ack(irq_data);
    }
}

pub(super) fn mask_irq(irq_data: &Arc<IrqData>) {
    if irq_data.common_data().masked() {
        return;
    }

    let chip = irq_data.chip_info_read_irqsave().chip();
    if chip.irq_mask(irq_data).is_ok() {
        irq_data.irqd_set(IrqStatus::IRQD_IRQ_MASKED);
    }
}

pub(super) fn unmask_irq(irq_data: &Arc<IrqData>) {
    if !irq_data.common_data().masked() {
        return;
    }

    let chip = irq_data.chip_info_read_irqsave().chip();

    if chip.irq_unmask(irq_data).is_ok() {
        irq_data.irqd_clear(IrqStatus::IRQD_IRQ_MASKED);
    }
}

impl IrqManager {
    pub(super) fn do_irq_wake_thread(
        &self,
        desc: &Arc<IrqDesc>,
        action_inner: &mut SpinLockGuard<'_, InnerIrqAction>,
    ) {
        let thread = action_inner.thread();

        if thread.is_none() {
            return;
        }

        let thread = thread.unwrap();
        if thread.flags().contains(ProcessFlags::EXITING) {
            return;
        }

        // 如果线程已经在运行，我们不需要唤醒它
        if action_inner
            .thread_flags_mut()
            .test_and_set_bit(ThreadedHandlerFlags::IRQTF_RUNTHREAD)
        {
            return;
        }

        desc.inc_threads_active();

        ProcessManager::wakeup(&thread).ok();
    }
}

fn handle_irq_event<'a>(
    irq_desc: &'a Arc<IrqDesc>,
    mut desc_inner_guard: SpinLockGuard<'_, InnerIrqDesc>,
) -> SpinLockGuard<'a, InnerIrqDesc> {
    desc_inner_guard
        .internal_state_mut()
        .remove(IrqDescState::IRQS_PENDING);
    desc_inner_guard.common_data().set_inprogress();

    drop(desc_inner_guard);

    let _r = do_handle_irq_event(irq_desc);

    let desc_inner_guard = irq_desc.inner();
    desc_inner_guard.common_data().clear_inprogress();

    return desc_inner_guard;
}
/// 处理中断事件
///
/// https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/handle.c?fi=handle_irq_event#139
#[inline(never)]
fn do_handle_irq_event(desc: &Arc<IrqDesc>) -> Result<(), SystemError> {
    let desc_inner_guard = desc.inner();
    let irq_data = desc_inner_guard.irq_data().clone();
    let actions = desc_inner_guard.actions().clone();
    drop(desc_inner_guard);

    let irq = irq_data.irq();
    let mut r = Ok(IrqReturn::NotHandled);

    for action in actions {
        let mut action_inner: SpinLockGuard<'_, InnerIrqAction> = action.inner();
        // debug!("do_handle_irq_event: action: {:?}", action_inner.name());
        let dynamic_data = action_inner
            .dev_id()
            .clone()
            .map(|d| d as Arc<dyn IrqHandlerData>);
        r = action_inner
            .handler()
            .unwrap()
            .handle(irq, None, dynamic_data);

        if let Ok(IrqReturn::WakeThread) = r {
            if unlikely(action_inner.thread_fn().is_none()) {
                warn_no_thread(irq, &mut action_inner);
            } else {
                irq_manager().do_irq_wake_thread(desc, &mut action_inner);
            }
        };
    }

    return r.map(|_| ());
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/chip.c?r=&mo=17578&fi=659
fn cond_unmask_eoi_irq(
    desc_inner_guard: &SpinLockGuard<'_, InnerIrqDesc>,
    chip: &Arc<dyn IrqChip>,
) {
    if !desc_inner_guard
        .internal_state()
        .contains(IrqDescState::IRQS_ONESHOT)
    {
        chip.irq_eoi(desc_inner_guard.irq_data());
        return;
    }

    /*
     * We need to unmask in the following cases:
     * - Oneshot irq which did not wake the thread (caused by a
     *   spurious interrupt or a primary handler handling it
     *   completely).
     */

    if !desc_inner_guard.common_data().disabled()
        && desc_inner_guard.common_data().masked()
        && desc_inner_guard.threads_oneshot() == 0
    {
        debug!(
            "eoi unmask irq {}",
            desc_inner_guard.irq_data().irq().data()
        );
        chip.irq_eoi(desc_inner_guard.irq_data());
        unmask_irq(desc_inner_guard.irq_data());
    } else if !chip.flags().contains(IrqChipFlags::IRQCHIP_EOI_THREADED) {
        debug!("eoi irq {}", desc_inner_guard.irq_data().irq().data());
        chip.irq_eoi(desc_inner_guard.irq_data());
    } else {
        warn!(
            "irq {} eoi failed",
            desc_inner_guard.irq_data().irq().data()
        );
    }
}

fn warn_no_thread(irq: IrqNumber, action_inner: &mut SpinLockGuard<'_, InnerIrqAction>) {
    // warn on once
    if action_inner
        .thread_flags_mut()
        .test_and_set_bit(ThreadedHandlerFlags::IRQTF_WARNED)
    {
        return;
    }

    warn!(
        "irq {}, device {} returned IRQ_WAKE_THREAD, but no threaded handler",
        irq.data(),
        action_inner.name()
    );
}

/// `handle_percpu_devid_irq` - 带有per-CPU设备id的perCPU本地中断处理程序
///
///
/// * `desc`: 此中断的中断描述结构
///
/// 在没有锁定要求的SMP机器上的每个CPU中断。与linux的`handle_percpu_irq()`相同，但有以下额外内容：
///
/// `action->percpu_dev_id`是一个指向per-cpu变量的指针，这些变量
/// 包含调用此处理程序的cpu的真实设备id
#[derive(Debug)]
pub struct PerCpuDevIdIrqHandler;

impl IrqFlowHandler for PerCpuDevIdIrqHandler {
    fn handle(&self, irq_desc: &Arc<IrqDesc>, _trap_frame: &mut TrapFrame) {
        let desc_inner_guard = irq_desc.inner();
        let irq_data = desc_inner_guard.irq_data().clone();
        let chip = irq_data.chip_info_read().chip();

        chip.irq_ack(&irq_data);

        let irq = irq_data.irq();

        let action = desc_inner_guard.actions().first().cloned();

        drop(desc_inner_guard);

        if let Some(action) = action {
            let action_inner = action.inner();
            let per_cpu_devid = action_inner.per_cpu_dev_id().cloned();

            let handler = action_inner.handler().unwrap();
            drop(action_inner);

            let _r = handler.handle(
                irq,
                None,
                per_cpu_devid.map(|d| d as Arc<dyn IrqHandlerData>),
            );
        } else {
            let cpu = smp_get_processor_id();

            let enabled = irq_desc
                .inner()
                .percpu_enabled()
                .as_ref()
                .unwrap()
                .get(cpu)
                .unwrap_or(false);

            if enabled {
                irq_manager().irq_percpu_disable(irq_desc, &irq_data, &chip, cpu);
            }
            static ONCE: Once = Once::new();

            ONCE.call_once(|| {
                error!(
                    "Spurious percpu irq {} on cpu {:?}, enabled: {}",
                    irq.data(),
                    cpu,
                    enabled
                );
            });
        }

        chip.irq_eoi(&irq_data);
    }
}
