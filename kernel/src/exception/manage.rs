use core::ops::{BitXor, Deref, DerefMut};

use alloc::{string::String, sync::Arc};

use system_error::SystemError;

use crate::{
    driver::base::device::DeviceId,
    exception::{
        irqchip::IrqChipSetMaskResult,
        irqdesc::{irq_desc_manager, InnerIrqDesc, IrqAction},
    },
    libs::{cpumask::CpuMask, spinlock::SpinLockGuard},
    process::{kthread::KernelThreadMechanism, ProcessManager},
    smp::cpu::ProcessorId,
};

use super::{
    dummychip::no_irq_chip,
    irqchip::IrqChipFlags,
    irqdata::{IrqData, IrqHandlerData, IrqLineStatus, IrqStatus},
    irqdesc::{InnerIrqAction, IrqDesc, IrqDescState, IrqHandleFlags, IrqHandler, IrqReturn},
    irqdomain::irq_domain_manager,
    IrqNumber,
};

lazy_static! {
    /// 默认的中断亲和性
    static ref IRQ_DEFAULT_AFFINITY: CpuMask = {
        let mut mask = CpuMask::new();
        // 默认情况下，中断处理程序将在第一个处理器上运行
        mask.set(ProcessorId::new(0), true);
        mask
    };
}

pub fn irq_manager() -> &'static IrqManager {
    &IrqManager
}

/// 中断管理器
pub struct IrqManager;

impl IrqManager {
    pub const IRQ_RESEND: bool = true;
    #[allow(dead_code)]
    pub const IRQ_NORESEND: bool = false;
    #[allow(dead_code)]
    pub const IRQ_START_FORCE: bool = true;
    pub const IRQ_START_COND: bool = false;

    /// 在中断线上添加一个处理函数
    ///
    /// ## 参数
    ///
    /// - irq: 虚拟中断号(中断线号)
    /// - name: 生成该中断的设备名称
    /// - handler: 中断处理函数
    /// - flags: 中断处理标志
    /// - dev_id: 一个用于标识设备的cookie
    pub fn request_irq(
        &self,
        irq: IrqNumber,
        name: String,
        handler: &'static dyn IrqHandler,
        flags: IrqHandleFlags,
        dev_id: Option<Arc<DeviceId>>,
    ) -> Result<(), SystemError> {
        return self.request_threaded_irq(irq, Some(handler), None, flags, name, dev_id);
    }

    /// 在中断线上添加一个处理函数（可以是线程化的中断）
    ///
    /// ## 参数
    ///
    /// - irq: 虚拟中断号
    /// - handler: 当中断发生时将被调用的函数，是
    ///     线程化中断的初级处理程序。如果handler为`None`并且thread_fn不为`None`，
    ///    将安装默认的初级处理程序
    /// - thread_fn: 在中断处理程序线程中调用的函数. 如果为`None`，则不会创建irq线程
    /// - flags: 中断处理标志
    ///     - IRQF_SHARED: 中断是共享的
    ///     - IRQF_TRIGGER*: 指定中断触发方式
    ///     - IRQF_ONESHOT: 在thread_fn中运行时，中断线被遮蔽
    /// - dev_name: 生成该中断的设备名称
    /// - dev_id: 一个用于标识设备的cookie
    ///
    /// ## 说明
    ///
    /// 此调用分配中断资源并启用中断线和IRQ处理。
    /// 从这一点开始，您的处理程序函数可能会被调用。
    /// 因此，您必须确保首先初始化您的硬件，
    /// 并确保以正确的顺序设置中断处理程序。
    ///
    /// 如果您想为您的设备设置线程化中断处理程序
    /// 则需要提供@handler和@thread_fn。@handler仍然
    /// 在硬中断上下文中调用，并且必须检查
    /// 中断是否来自设备。如果是，它需要禁用设备上的中断
    /// 并返回IRQ_WAKE_THREAD，这将唤醒处理程序线程并运行
    /// @thread_fn。这种拆分处理程序设计是为了支持
    /// 共享中断。
    ///
    /// dev_id必须是全局唯一的。通常使用设备数据结构的地址或者uuid
    /// 作为cookie。由于处理程序接收这个值，因此使用它是有意义的。
    ///
    /// 如果您的中断是共享的，您必须传递一个非NULL的dev_id
    /// 因为当释放中断时需要它。
    pub fn request_threaded_irq(
        &self,
        irq: IrqNumber,
        mut handler: Option<&'static dyn IrqHandler>,
        thread_fn: Option<&'static dyn IrqHandler>,
        flags: IrqHandleFlags,
        dev_name: String,
        dev_id: Option<Arc<DeviceId>>,
    ) -> Result<(), SystemError> {
        if irq == IrqNumber::IRQ_NOTCONNECTED {
            return Err(SystemError::ENOTCONN);
        }

        // 逻辑检查：共享中断必须传入一个真正的设备ID，
        // 否则后来我们将难以确定哪个中断是哪个（会搞乱中断释放逻辑等）。
        // 此外，共享中断与禁用自动使能不相符。 共享中断可能在仍然禁用时请求它，然后永远等待中断。
        // 另外，IRQF_COND_SUSPEND 仅适用于共享中断，并且它不能与 IRQF_NO_SUSPEND 同时设置。

        if ((flags.contains(IrqHandleFlags::IRQF_SHARED)) && dev_id.is_none())
            || ((flags.contains(IrqHandleFlags::IRQF_SHARED))
                && (flags.contains(IrqHandleFlags::IRQF_NO_AUTOEN)))
            || (!(flags.contains(IrqHandleFlags::IRQF_SHARED))
                && (flags.contains(IrqHandleFlags::IRQF_COND_SUSPEND)))
            || ((flags.contains(IrqHandleFlags::IRQF_NO_SUSPEND))
                && (flags.contains(IrqHandleFlags::IRQF_COND_SUSPEND)))
        {
            return Err(SystemError::EINVAL);
        }
        let desc = irq_desc_manager().lookup(irq).ok_or(SystemError::EINVAL)?;
        if !desc.can_request() {
            kwarn!("desc {} can not request", desc.irq().data());
            return Err(SystemError::EINVAL);
        }

        if handler.is_none() {
            if thread_fn.is_none() {
                // 不允许中断处理函数和线程处理函数都为空
                return Err(SystemError::EINVAL);
            }

            // 如果中断处理函数为空，线程处理函数不为空，则使用默认的中断处理函数
            handler = Some(&DefaultPrimaryIrqHandler);
        }

        let irqaction = IrqAction::new(irq, dev_name, handler, thread_fn);

        let mut action_guard = irqaction.inner();
        *action_guard.flags_mut() = flags;
        *action_guard.dev_id_mut() = dev_id;
        drop(action_guard);

        return self.inner_setup_irq(irq, irqaction, desc);
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/manage.c?r=&mo=59252&fi=2138#1497
    #[inline(never)]
    fn inner_setup_irq(
        &self,
        irq: IrqNumber,
        action: Arc<IrqAction>,
        desc: Arc<IrqDesc>,
    ) -> Result<(), SystemError> {
        // ==== 定义错误处理函数 ====
        let err_out_thread =
            |e: SystemError, mut action_guard: SpinLockGuard<'_, InnerIrqAction>| -> SystemError {
                if let Some(thread_pcb) = action_guard.thread() {
                    action_guard.set_thread(None);
                    KernelThreadMechanism::stop(&thread_pcb).ok();
                }

                if let Some(secondary) = action_guard.secondary() {
                    let mut secondary_guard = secondary.inner();
                    if let Some(thread_pcb) = secondary_guard.thread() {
                        secondary_guard.set_thread(None);
                        KernelThreadMechanism::stop(&thread_pcb).ok();
                    }
                }
                return e;
            };

        let err_out_bus_unlock = |e: SystemError,
                                  desc: Arc<IrqDesc>,
                                  req_mutex_guard: crate::libs::mutex::MutexGuard<'_, ()>,
                                  action_guard: SpinLockGuard<'_, InnerIrqAction>|
         -> SystemError {
            desc.chip_bus_sync_unlock();
            drop(req_mutex_guard);
            return err_out_thread(e, action_guard);
        };

        let err_out_unlock = |e: SystemError,
                              desc_guard: SpinLockGuard<'_, InnerIrqDesc>,
                              desc: Arc<IrqDesc>,
                              req_mutex_guard: crate::libs::mutex::MutexGuard<'_, ()>,
                              action_guard: SpinLockGuard<'_, InnerIrqAction>|
         -> SystemError {
            drop(desc_guard);
            return err_out_bus_unlock(e, desc, req_mutex_guard, action_guard);
        };

        let err_out_mismatch = |old_action_guard: SpinLockGuard<'_, InnerIrqAction>,
                                desc_guard: SpinLockGuard<'_, InnerIrqDesc>,
                                action_guard: SpinLockGuard<'_, InnerIrqAction>,
                                desc: Arc<IrqDesc>,
                                req_mutex_guard: crate::libs::mutex::MutexGuard<'_, ()>|
         -> SystemError {
            if !action_guard
                .flags()
                .contains(IrqHandleFlags::IRQF_PROBE_SHARED)
            {
                kerror!("Flags mismatch for irq {} (name: {}, flags: {:?}). old action name: {}, old flags: {:?}", irq.data(), action_guard.name(), action_guard.flags(), old_action_guard.name(), old_action_guard.flags());
            }
            return err_out_unlock(
                SystemError::EBUSY,
                desc_guard,
                desc,
                req_mutex_guard,
                action_guard,
            );
        };

        // ===== 代码开始 =====

        if Arc::ptr_eq(
            &desc.irq_data().chip_info_read_irqsave().chip(),
            &no_irq_chip(),
        ) {
            return Err(SystemError::ENOSYS);
        }

        let mut action_guard = action.inner();
        if !action_guard.flags().trigger_type_specified() {
            // 如果没有指定触发类型，则使用默认的触发类型
            action_guard
                .flags_mut()
                .insert_trigger_type(desc.irq_data().common_data().trigger_type())
        }

        let nested = desc.nested_thread();

        if nested {
            if action_guard.thread_fn().is_none() {
                return Err(SystemError::EINVAL);
            }

            action_guard.set_handler(Some(&IrqNestedPrimaryHandler));
        } else {
            if desc.can_thread() {
                self.setup_forced_threading(action_guard.deref_mut())?;
            }
        }

        // 如果具有中断线程处理程序，并且中断不是嵌套的，则设置中断线程
        if action_guard.thread_fn().is_some() && !nested {
            self.setup_irq_thread(irq, action_guard.deref(), false)?;

            if let Some(secondary) = action_guard.secondary() {
                let secondary_guard = secondary.inner();
                if let Err(e) = self.setup_irq_thread(irq, secondary_guard.deref(), true) {
                    return Err(err_out_thread(e, action_guard));
                }
            }
        }

        // Drivers are often written to work w/o knowledge about the
        // underlying irq chip implementation, so a request for a
        // threaded irq without a primary hard irq context handler
        // requires the ONESHOT flag to be set. Some irq chips like
        // MSI based interrupts are per se one shot safe. Check the
        // chip flags, so we can avoid the unmask dance at the end of
        // the threaded handler for those.

        if desc
            .irq_data()
            .chip_info_read_irqsave()
            .chip()
            .flags()
            .contains(IrqChipFlags::IRQCHIP_ONESHOT_SAFE)
        {
            *action_guard.flags_mut() &= !IrqHandleFlags::IRQF_ONESHOT;
        }

        // Protects against a concurrent __free_irq() call which might wait
        // for synchronize_hardirq() to complete without holding the optional
        // chip bus lock and desc->lock. Also protects against handing out
        // a recycled oneshot thread_mask bit while it's still in use by
        // its previous owner.
        let req_mutex_guard = desc.request_mutex_lock();

        // Acquire bus lock as the irq_request_resources() callback below
        // might rely on the serialization or the magic power management
        // functions which are abusing the irq_bus_lock() callback,
        desc.chip_bus_lock();

        // 如果当前中断线上还没有irqaction, 则先为中断线申请资源
        if desc.actions().is_empty() {
            if let Err(e) = self.irq_request_resources(desc.clone()) {
                kerror!(
                    "Failed to request resources for {} (irq {}) on irqchip {}, error {:?}",
                    action_guard.name(),
                    irq.data(),
                    desc.irq_data().chip_info_read_irqsave().chip().name(),
                    e
                );
                return Err(err_out_bus_unlock(
                    e,
                    desc.clone(),
                    req_mutex_guard,
                    action_guard,
                ));
            }
        }

        let mut desc_inner_guard: SpinLockGuard<'_, InnerIrqDesc> = desc.inner();

        // 标记当前irq是否是共享的
        let mut irq_shared = false;
        if desc_inner_guard.actions().is_empty() == false {
            // 除非双方都同意并且是相同类型（级别、边沿、极性），否则不能共享中断。
            // 因此，两个标志字段都必须设置IRQF_SHARED，并且设置触发类型的位必须匹配。
            // 另外，所有各方都必须就ONESHOT达成一致。
            // NMI用途的中断线不能共享。
            if desc_inner_guard
                .internal_state()
                .contains(IrqDescState::IRQS_NMI)
            {
                kerror!(
                    "Invalid attempt to share NMI for {} (irq {}) on irqchip {}",
                    action_guard.name(),
                    irq.data(),
                    desc_inner_guard
                        .irq_data()
                        .chip_info_read_irqsave()
                        .chip()
                        .name()
                );
                return Err(err_out_unlock(
                    SystemError::EINVAL,
                    desc_inner_guard,
                    desc.clone(),
                    req_mutex_guard,
                    action_guard,
                ));
            }

            let irq_data = desc_inner_guard.irq_data();

            let old_trigger_type: super::irqdata::IrqLineStatus;
            let status = irq_data.common_data().status();
            if status.trigger_type_was_set() {
                old_trigger_type = status.trigger_type();
            } else {
                old_trigger_type = action_guard.flags().trigger_type();
                irq_data.common_data().set_trigger_type(old_trigger_type);
            }

            let old = &desc_inner_guard.actions()[0].clone();
            let old_guard = old.inner();

            if ((old_guard
                .flags()
                .intersection(*action_guard.flags())
                .contains(IrqHandleFlags::IRQF_SHARED))
                == false)
                || (old_trigger_type != (action_guard.flags().trigger_type()))
                || ((old_guard.flags().bitxor(*action_guard.flags()))
                    .contains(IrqHandleFlags::IRQF_ONESHOT))
            {
                return Err(err_out_mismatch(
                    old_guard,
                    desc_inner_guard,
                    action_guard,
                    desc.clone(),
                    req_mutex_guard,
                ));
            }

            // all handlers must agree on per-cpuness
            if *old_guard.flags() & IrqHandleFlags::IRQF_PERCPU
                != *action_guard.flags() & IrqHandleFlags::IRQF_PERCPU
            {
                return Err(err_out_mismatch(
                    old_guard,
                    desc_inner_guard,
                    action_guard,
                    desc.clone(),
                    req_mutex_guard,
                ));
            }

            irq_shared = true;
        }

        if action_guard.flags().contains(IrqHandleFlags::IRQF_ONESHOT) {
            // todo: oneshot
        } else if action_guard.handler().is_some_and(|h| {
            h.type_id() == (&DefaultPrimaryIrqHandler as &dyn IrqHandler).type_id()
        }) && desc_inner_guard
            .irq_data()
            .chip_info_read_irqsave()
            .chip()
            .flags()
            .contains(IrqChipFlags::IRQCHIP_ONESHOT_SAFE)
            == false
        {
            // 请求中断时 hander = NULL，因此我们为其使用默认的主处理程序。
            // 但它没有设置ONESHOT标志。与电平触发中断结合时，
            // 这是致命的，因为默认的主处理程序只是唤醒线程，然后重新启用 irq 线路，
            // 但设备仍然保持电平中断生效。周而复始....
            // 虽然这对于边缘类型中断来说可行，但我们为了安全起见，不加条件地拒绝，
            // 因为我们不能确定这个中断实际上具有什么类型。
            // 由于底层芯片实现可能会覆盖它们，所以类型标志并不可靠.

            kerror!(
                "Requesting irq {} without a handler, and ONESHOT flags not set for irqaction: {}",
                irq.data(),
                action_guard.name()
            );
            return Err(err_out_unlock(
                SystemError::EINVAL,
                desc_inner_guard,
                desc.clone(),
                req_mutex_guard,
                action_guard,
            ));
        }

        // 第一次在当前irqdesc上注册中断处理函数
        if !irq_shared {
            // 设置中断触发方式
            if action_guard.flags().trigger_type_specified() {
                let trigger_type = action_guard.flags().trigger_type();
                if let Err(e) =
                    self.do_set_irq_trigger(desc.clone(), &mut desc_inner_guard, trigger_type)
                {
                    return Err(err_out_unlock(
                        e,
                        desc_inner_guard,
                        desc.clone(),
                        req_mutex_guard,
                        action_guard,
                    ));
                }
            }

            // 激活中断。这种激活必须独立于IRQ_NOAUTOEN进行*desc_inner_guard.internal_state_mut() |= IrqDescState::IRQS_NOREQUEST;uest.
            if let Err(e) = self.irq_activate(&desc, &mut desc_inner_guard) {
                return Err(err_out_unlock(
                    e,
                    desc_inner_guard,
                    desc.clone(),
                    req_mutex_guard,
                    action_guard,
                ));
            }

            *desc_inner_guard.internal_state_mut() &= !(IrqDescState::IRQS_AUTODETECT
                | IrqDescState::IRQS_SPURIOUS_DISABLED
                | IrqDescState::IRQS_ONESHOT
                | IrqDescState::IRQS_WAITING);
            desc_inner_guard
                .common_data()
                .clear_status(IrqStatus::IRQD_IRQ_INPROGRESS);

            if action_guard.flags().contains(IrqHandleFlags::IRQF_PERCPU) {
                desc_inner_guard
                    .common_data()
                    .insert_status(IrqStatus::IRQD_PER_CPU);
                desc_inner_guard.line_status_set_per_cpu();

                if action_guard.flags().contains(IrqHandleFlags::IRQF_NO_DEBUG) {
                    desc_inner_guard.line_status_set_no_debug();
                }
            }

            if action_guard.flags().contains(IrqHandleFlags::IRQF_ONESHOT) {
                *desc_inner_guard.internal_state_mut() |= IrqDescState::IRQS_ONESHOT;
            }

            // 如果有要求的话，则忽略IRQ的均衡。
            if action_guard
                .flags()
                .contains(IrqHandleFlags::IRQF_NOBALANCING)
            {
                todo!("IRQF_NO_BALANCING");
            }

            if !action_guard
                .flags()
                .contains(IrqHandleFlags::IRQF_NO_AUTOEN)
                && desc_inner_guard.can_autoenable()
            {
                // 如果没有设置IRQF_NOAUTOEN，则自动使能中断
                self.irq_startup(
                    &desc,
                    &mut desc_inner_guard,
                    Self::IRQ_RESEND,
                    Self::IRQ_START_COND,
                )
                .ok();
            } else {
                // 共享中断与禁用自动使能不太兼容。
                // 共享中断可能在它仍然被禁用时请求它，然后永远等待中断。

                static mut WARNED: bool = false;
                if action_guard.flags().contains(IrqHandleFlags::IRQF_SHARED) {
                    if unsafe { !WARNED } {
                        kwarn!(
                            "Shared interrupt {} for {} requested but not auto enabled",
                            irq.data(),
                            action_guard.name()
                        );
                        unsafe { WARNED = true };
                    }
                }

                desc_inner_guard.set_depth(1);
            }
        } else if action_guard.flags().trigger_type_specified() {
            let new_trigger_type = action_guard.flags().trigger_type();
            let old_trigger_type = desc_inner_guard.common_data().trigger_type();
            if new_trigger_type != old_trigger_type {
                kwarn!("Irq {} uses trigger type: {old_trigger_type:?}, but requested trigger type: {new_trigger_type:?}.", irq.data());
            }
        }

        // 在队列末尾添加新的irqaction
        desc_inner_guard.add_action(action.clone());

        // 检查我们是否曾经通过虚构的中断处理程序禁用过irq。重新启用它并再给它一次机会。
        if irq_shared
            && desc_inner_guard
                .internal_state()
                .contains(IrqDescState::IRQS_SPURIOUS_DISABLED)
        {
            desc_inner_guard
                .internal_state_mut()
                .remove(IrqDescState::IRQS_SPURIOUS_DISABLED);
            self.do_enable_irq(desc.clone(), &mut desc_inner_guard).ok();
        }

        drop(desc_inner_guard);
        desc.chip_bus_sync_unlock();
        drop(req_mutex_guard);

        drop(action_guard);
        self.wake_up_and_wait_for_irq_thread_ready(&desc, Some(action.clone()));
        self.wake_up_and_wait_for_irq_thread_ready(&desc, action.inner().secondary());
        return Ok(());
    }

    /// 唤醒中断线程并等待中断线程准备好
    ///
    /// ## 参数
    ///
    /// - desc: 中断描述符
    /// - action: 要唤醒的中断处理函数
    ///
    /// ## 锁
    ///
    /// 进入当前函数时，`action`的锁需要被释放
    fn wake_up_and_wait_for_irq_thread_ready(
        &self,
        desc: &Arc<IrqDesc>,
        action: Option<Arc<IrqAction>>,
    ) {
        if action.is_none() {
            return;
        }

        let action = action.unwrap();

        let action_guard = action.inner();
        if action_guard.thread().is_none() {
            return;
        }

        ProcessManager::wakeup(&action_guard.thread().unwrap()).ok();
        drop(action_guard);
        action
            .thread_completion()
            .wait_for_completion()
            .map_err(|e| {
                kwarn!(
                    "Failed to wait for irq thread ready for {} (irq {:?}), error {:?}",
                    action.inner().name(),
                    desc.irq_data().irq(),
                    e
                );
            })
            .ok();
    }

    pub(super) fn irq_activate_and_startup(
        &self,
        desc: &Arc<IrqDesc>,
        desc_inner_guard: &mut SpinLockGuard<'_, InnerIrqDesc>,
        resend: bool,
    ) -> Result<(), SystemError> {
        self.irq_activate(desc, desc_inner_guard)?;
        self.irq_startup(desc, desc_inner_guard, resend, Self::IRQ_START_FORCE)
    }

    pub(super) fn irq_activate(
        &self,
        _desc: &Arc<IrqDesc>,
        desc_inner_guard: &mut SpinLockGuard<'_, InnerIrqDesc>,
    ) -> Result<(), SystemError> {
        let irq_data = desc_inner_guard.irq_data();

        if !desc_inner_guard.common_data().status().affinity_managed() {
            return irq_domain_manager().activate_irq(irq_data, false);
        }

        return Ok(());
    }

    /// 设置CPU亲和性并开启中断
    pub(super) fn irq_startup(
        &self,
        desc: &Arc<IrqDesc>,
        desc_inner_guard: &mut SpinLockGuard<'_, InnerIrqDesc>,
        resend: bool,
        force: bool,
    ) -> Result<(), SystemError> {
        let mut ret = Ok(());
        let irq_data = desc_inner_guard.irq_data().clone();
        let affinity = desc_inner_guard.common_data().affinity();
        desc_inner_guard.set_depth(0);

        if desc_inner_guard.common_data().status().started() {
            self.irq_enable(desc_inner_guard);
        } else {
            match self.__irq_startup_managed(desc_inner_guard, &affinity, force) {
                IrqStartupResult::Normal => {
                    if irq_data
                        .chip_info_read_irqsave()
                        .chip()
                        .flags()
                        .contains(IrqChipFlags::IRQCHIP_AFFINITY_PRE_STARTUP)
                    {
                        self.irq_setup_affinity(desc, desc_inner_guard).ok();
                    }

                    ret = self.__irq_startup(desc_inner_guard);

                    if !irq_data
                        .chip_info_read_irqsave()
                        .chip()
                        .flags()
                        .contains(IrqChipFlags::IRQCHIP_AFFINITY_PRE_STARTUP)
                    {
                        self.irq_setup_affinity(desc, desc_inner_guard).ok();
                    }
                }
                IrqStartupResult::Managed => {
                    self.irq_do_set_affinity(&irq_data, &desc_inner_guard, &affinity, false)
                        .ok();
                    ret = self.__irq_startup(desc_inner_guard);
                }
                IrqStartupResult::Abort => {
                    desc_inner_guard
                        .common_data()
                        .insert_status(IrqStatus::IRQD_MANAGED_SHUTDOWN);
                    return Ok(());
                }
            }
        }

        if resend {
            if let Err(e) = self.irq_check_and_resend(desc_inner_guard, false) {
                kerror!(
                    "Failed to check and resend irq {}, error {:?}",
                    irq_data.irq().data(),
                    e
                );
            }
        }

        return ret;
    }

    pub fn irq_enable(&self, desc_inner_guard: &SpinLockGuard<'_, InnerIrqDesc>) {
        let common_data = desc_inner_guard.common_data();
        if !common_data.status().disabled() {
            self.unmask_irq(desc_inner_guard);
        } else {
            common_data.clear_disabled();

            let chip = desc_inner_guard.irq_data().chip_info_read_irqsave().chip();

            if let Err(e) = chip.irq_enable(&desc_inner_guard.irq_data()) {
                if e == SystemError::ENOSYS {
                    self.unmask_irq(desc_inner_guard);
                }
                kerror!(
                    "Failed to enable irq {} (name: {:?}), error {:?}",
                    desc_inner_guard.irq_data().irq().data(),
                    desc_inner_guard.name(),
                    e
                );
            } else {
                common_data.clear_masked();
            }
        }
    }

    /// 自动设置中断的CPU亲和性
    ///
    ///  
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/manage.c#589
    pub fn irq_setup_affinity(
        &self,
        _desc: &Arc<IrqDesc>,
        desc_inner_guard: &mut SpinLockGuard<'_, InnerIrqDesc>,
    ) -> Result<(), SystemError> {
        let common_data = desc_inner_guard.common_data();
        if !desc_inner_guard.can_set_affinity() {
            return Ok(());
        }

        let mut to_set = IRQ_DEFAULT_AFFINITY.clone();
        if common_data.status().affinity_managed()
            || common_data.status().contains(IrqStatus::IRQD_AFFINITY_SET)
        {
            // FIXME: 要判断affinity跟已上线的CPU是否有交集

            let irq_aff = common_data.affinity();
            if irq_aff.is_empty() {
                common_data.clear_status(IrqStatus::IRQD_AFFINITY_SET);
            } else {
                to_set = irq_aff;
            }
        }

        // FIXME: 求to_set和在线CPU的交集

        return self.irq_do_set_affinity(
            desc_inner_guard.irq_data(),
            &desc_inner_guard,
            &to_set,
            false,
        );
    }

    pub fn irq_do_set_affinity(
        &self,
        irq_data: &Arc<IrqData>,
        desc_inner_guard: &SpinLockGuard<'_, InnerIrqDesc>,
        cpumask: &CpuMask,
        force: bool,
    ) -> Result<(), SystemError> {
        let chip = irq_data.chip_info_read_irqsave().chip();
        if !chip.can_set_affinity() {
            return Err(SystemError::EINVAL);
        }

        // todo: 处理CPU中断隔离相关的逻辑

        let common_data = desc_inner_guard.common_data();
        let r;
        if !force && !cpumask.is_empty() {
            r = chip.irq_set_affinity(irq_data, &cpumask, force);
        } else if force {
            r = chip.irq_set_affinity(irq_data, &cpumask, force);
        } else {
            return Err(SystemError::EINVAL);
        }

        let mut ret = Ok(());
        if let Ok(rs) = r {
            match rs {
                IrqChipSetMaskResult::SetMaskOk | IrqChipSetMaskResult::SetMaskOkDone => {
                    common_data.set_affinity(cpumask.clone());
                }
                IrqChipSetMaskResult::SetMaskOkNoChange => {

                    // irq_validate_effective_affinity(data);
                    // irq_set_thread_affinity(desc);
                }
            }
        } else {
            ret = Err(r.unwrap_err());
        }

        return ret;
    }

    fn __irq_startup(
        &self,
        desc_inner_guard: &SpinLockGuard<'_, InnerIrqDesc>,
    ) -> Result<(), SystemError> {
        let common_data = desc_inner_guard.common_data();

        if let Err(e) = desc_inner_guard
            .irq_data()
            .chip_info_read_irqsave()
            .chip()
            .irq_startup(desc_inner_guard.irq_data())
        {
            if e == SystemError::ENOSYS {
                self.irq_enable(desc_inner_guard);
            } else {
                return Err(e);
            }
        } else {
            common_data.clear_disabled();
            common_data.clear_masked();
        }

        common_data.set_started();

        return Ok(());
    }

    fn __irq_startup_managed(
        &self,
        desc_inner_guard: &SpinLockGuard<'_, InnerIrqDesc>,
        _affinity: &CpuMask,
        _force: bool,
    ) -> IrqStartupResult {
        let irq_data = desc_inner_guard.irq_data();
        let common_data = desc_inner_guard.common_data();

        if !common_data.status().affinity_managed() {
            return IrqStartupResult::Normal;
        }

        common_data.clear_managed_shutdown();

        /*
            - 检查Affinity掩码是否包括所有的在线CPU。如果是，这意味着有代码试图在管理的中断上使用enable_irq()，
                这可能是非法的。在这种情况下，如果force不是真值，函数会返回IRQ_STARTUP_ABORT，表示中断处理应该被放弃。
            - 如果Affinity掩码中没有任何在线的CPU，那么中断请求是不可用的，因为没有任何CPU可以处理它。
                在这种情况下，如果force不是真值，函数同样会返回IRQ_STARTUP_ABORT。
            - 如果以上条件都不满足，尝试激活中断，并将其设置为管理模式。这是通过调用 `irq_domain_manager().activate_irq()` 函数来实现的。
                如果这个调用失败，表示有保留的资源无法访问，函数会返回IRQ_STARTUP_ABORT。
            - 如果一切顺利，函数会返回IRQ_STARTUP_MANAGED，表示中断已经被成功管理并激活。
        */

        // if (cpumask_any_and(aff, cpu_online_mask) >= nr_cpu_ids) {
        //     /*
        //      * Catch code which fiddles with enable_irq() on a managed
        //      * and potentially shutdown IRQ. Chained interrupt
        //      * installment or irq auto probing should not happen on
        //      * managed irqs either.
        //      */
        //     if (WARN_ON_ONCE(force))
        //         return IRQ_STARTUP_ABORT;
        //     /*
        //      * The interrupt was requested, but there is no online CPU
        //      * in it's affinity mask. Put it into managed shutdown
        //      * state and let the cpu hotplug mechanism start it up once
        //      * a CPU in the mask becomes available.
        //      */
        //     return IRQ_STARTUP_ABORT;
        // }

        let r = irq_domain_manager().activate_irq(irq_data, false);
        if r.is_err() {
            return IrqStartupResult::Abort;
        }

        return IrqStartupResult::Managed;
    }

    pub fn do_enable_irq(
        &self,
        _desc: Arc<IrqDesc>,
        _desc_inner_guard: &mut SpinLockGuard<'_, InnerIrqDesc>,
    ) -> Result<(), SystemError> {
        // https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/manage.c?r=&mo=59252&fi=2138#776
        todo!("do_enable_irq")
    }

    #[inline(never)]
    pub fn do_set_irq_trigger(
        &self,
        _desc: Arc<IrqDesc>,
        desc_inner_guard: &mut SpinLockGuard<'_, InnerIrqDesc>,
        mut trigger_type: IrqLineStatus,
    ) -> Result<(), SystemError> {
        let chip = desc_inner_guard.irq_data().chip_info_read_irqsave().chip();
        let mut to_unmask = false;

        if !chip.can_set_flow_type() {
            // kdebug!(
            //     "No set_irq_type function for irq {}, chip {}",
            //     desc_inner_guard.irq_data().irq().data(),
            //     chip.name()
            // );
            return Ok(());
        }

        if chip.flags().contains(IrqChipFlags::IRQCHIP_SET_TYPE_MASKED) {
            if desc_inner_guard.common_data().status().masked() == false {
                self.mask_irq(desc_inner_guard.irq_data());
            }
            if desc_inner_guard.common_data().status().disabled() == false {
                to_unmask = true;
            }
        }

        trigger_type &= IrqLineStatus::IRQ_TYPE_SENSE_MASK;

        let r = chip.irq_set_type(desc_inner_guard.irq_data(), trigger_type);
        let ret;
        if let Ok(rs) = r {
            match rs {
                IrqChipSetMaskResult::SetMaskOk | IrqChipSetMaskResult::SetMaskOkDone => {
                    let common_data = desc_inner_guard.common_data();
                    common_data.clear_status(IrqStatus::IRQD_TRIGGER_MASK);
                    let mut irqstatus = IrqStatus::empty();
                    irqstatus.set_trigger_type(trigger_type);
                    common_data.insert_status(irqstatus);
                }
                IrqChipSetMaskResult::SetMaskOkNoChange => {
                    let flags = desc_inner_guard.common_data().trigger_type();
                    desc_inner_guard.set_trigger_type(flags);
                    desc_inner_guard
                        .common_data()
                        .clear_status(IrqStatus::IRQD_LEVEL);
                    desc_inner_guard.clear_level();

                    if (flags & IrqLineStatus::IRQ_TYPE_LEVEL_MASK).is_empty() == false {
                        desc_inner_guard.set_level();
                        desc_inner_guard
                            .common_data()
                            .insert_status(IrqStatus::IRQD_LEVEL);
                    }
                }
            }

            ret = Ok(());
        } else {
            kerror!(
                "Failed to set irq {} trigger type to {:?} on irqchip {}, error {:?}",
                desc_inner_guard.irq_data().irq().data(),
                trigger_type,
                chip.name(),
                r
            );

            ret = Err(r.unwrap_err());
        }

        if to_unmask {
            self.unmask_irq(desc_inner_guard);
        }
        return ret;
    }

    fn irq_request_resources(&self, desc: Arc<IrqDesc>) -> Result<(), SystemError> {
        let irq_data = desc.irq_data();
        let irq_chip = irq_data.chip_info_read_irqsave().chip();
        irq_chip.irq_request_resources(&irq_data)
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/manage.c?r=&mo=59252&fi=2138#1448
    fn setup_irq_thread(
        &self,
        _irq: IrqNumber,
        _action: &InnerIrqAction,
        _secondary: bool,
    ) -> Result<(), SystemError> {
        // if secondary {
        //     KernelThreadMechanism::create(func, name)
        // }

        todo!("setup_irq_thread")
    }

    fn setup_forced_threading(&self, _action: &mut InnerIrqAction) -> Result<(), SystemError> {
        // todo: 处理强制线程化的逻辑，参考linux的`irq_setup_forced_threading()`
        return Ok(());
    }

    pub fn irq_clear_status_flags(
        &self,
        irq: IrqNumber,
        status: IrqLineStatus,
    ) -> Result<(), SystemError> {
        let desc = irq_desc_manager().lookup(irq).ok_or(SystemError::EINVAL)?;
        desc.modify_status(status, IrqLineStatus::empty());
        return Ok(());
    }

    /// 屏蔽中断
    pub(super) fn mask_irq(&self, irq_data: &Arc<IrqData>) {
        if irq_data.common_data().status().masked() {
            return;
        }

        let chip = irq_data.chip_info_read_irqsave().chip();
        let r = chip.irq_mask(irq_data);

        if r.is_ok() {
            irq_data.common_data().set_masked();
        }
    }

    /// 解除屏蔽中断
    pub(super) fn unmask_irq(&self, desc_inner_guard: &SpinLockGuard<'_, InnerIrqDesc>) {
        if desc_inner_guard.common_data().status().masked() == false {
            return;
        }

        let r = desc_inner_guard
            .irq_data()
            .chip_info_read_irqsave()
            .chip()
            .irq_unmask(&desc_inner_guard.irq_data());

        if let Err(e) = r {
            if e != SystemError::ENOSYS {
                kerror!(
                    "Failed to unmask irq {} on irqchip {}, error {:?}",
                    desc_inner_guard.irq_data().irq().data(),
                    desc_inner_guard
                        .irq_data()
                        .chip_info_read_irqsave()
                        .chip()
                        .name(),
                    e
                );
            }
        } else {
            desc_inner_guard
                .common_data()
                .clear_status(IrqStatus::IRQD_IRQ_MASKED);
        }
    }

    /// 释放使用request_irq分配的中断
    ///
    /// ## 参数
    ///
    /// - irq: 要释放的中断线
    /// - dev_id: 要释放的设备身份
    ///
    /// ## 返回
    ///
    /// 返回传递给request_irq的devname参数
    ///
    /// ## 说明
    ///
    /// 移除一个中断处理程序。处理程序被移除，如果该中断线不再被任何驱动程序使用，则会被禁用。
    ///
    /// 在共享IRQ的情况下，调用者必须确保在调用此功能之前，它在所驱动的卡上禁用了中断。
    ///
    /// ## 注意
    ///
    /// 此函数不可以在中断上下文中调用。
    pub fn free_irq(&self, _irq: IrqNumber, _dev_id: Option<Arc<DeviceId>>) {
        kwarn!("Unimplemented free_irq");
    }
}

enum IrqStartupResult {
    Normal,
    Managed,
    Abort,
}
/// 默认的初级中断处理函数
///
/// 该处理函数仅仅返回`WakeThread`，即唤醒中断线程
#[derive(Debug)]
struct DefaultPrimaryIrqHandler;

impl IrqHandler for DefaultPrimaryIrqHandler {
    fn handle(
        &self,
        _irq: IrqNumber,
        _static_data: Option<&dyn IrqHandlerData>,
        _dynamic_data: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<IrqReturn, SystemError> {
        return Ok(IrqReturn::WakeThread);
    }
}

/// Primary handler for nested threaded interrupts.
/// Should never be called.
#[derive(Debug)]
struct IrqNestedPrimaryHandler;

impl IrqHandler for IrqNestedPrimaryHandler {
    fn handle(
        &self,
        irq: IrqNumber,
        _static_data: Option<&dyn IrqHandlerData>,
        _dynamic_data: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<IrqReturn, SystemError> {
        kwarn!("Primary handler called for nested irq {}", irq.data());
        return Ok(IrqReturn::NotHandled);
    }
}
