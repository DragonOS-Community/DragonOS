use system_error::SystemError;

use crate::{exception::irqdesc::IrqDescState, libs::spinlock::SpinLockGuard};

use super::{irqdesc::InnerIrqDesc, manage::IrqManager};

impl IrqManager {
    /// 检查状态并重发中断
    ///
    /// ## 参数
    ///
    /// - `desc_inner_guard`：中断描述符的锁
    /// - `inject`：是否注入中断
    pub(super) fn irq_check_and_resend(
        &self,
        desc_inner_guard: &mut SpinLockGuard<'_, InnerIrqDesc>,
        inject: bool,
    ) -> Result<(), SystemError> {
        // https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/resend.c?fi=check_irq_resend#106

        /*
         * 我们不重新发送电平触发类型的中断。电平触发类型的中断在它们仍然活动时由硬件重新发送。
         * 清除PENDING bit，以避免suspend/resume过程中的混淆。
         */
        if desc_inner_guard
            .common_data()
            .trigger_type()
            .is_level_type()
        {
            desc_inner_guard
                .internal_state_mut()
                .remove(IrqDescState::IRQS_PENDING);
            return Err(SystemError::EINVAL);
        }

        if desc_inner_guard
            .internal_state()
            .contains(IrqDescState::IRQS_REPLAY)
        {
            return Err(SystemError::EBUSY);
        }

        if desc_inner_guard
            .internal_state()
            .contains(IrqDescState::IRQS_PENDING)
            == false
            && inject == false
        {
            return Ok(());
        }

        desc_inner_guard
            .internal_state_mut()
            .remove(IrqDescState::IRQS_PENDING);

        let mut ret = Ok(());
        if self.try_retrigger(desc_inner_guard).is_err() {
            // todo: 支持发送到tasklet
            ret = Err(SystemError::EINVAL);
        }

        if ret.is_ok() {
            desc_inner_guard
                .internal_state_mut()
                .insert(IrqDescState::IRQS_REPLAY);
        }

        return ret;
    }

    fn try_retrigger(
        &self,
        desc_inner_guard: &SpinLockGuard<'_, InnerIrqDesc>,
    ) -> Result<(), SystemError> {
        if let Err(e) = desc_inner_guard
            .irq_data()
            .chip_info_read_irqsave()
            .chip()
            .retrigger(desc_inner_guard.irq_data())
        {
            if e != SystemError::ENOSYS {
                return Err(e);
            }
        } else {
            return Ok(());
        }

        // 当前中断控制器不支持重发中断，从父中断控制器重发
        return self.irq_chip_retrigger_hierarchy(desc_inner_guard.irq_data());
    }
}
