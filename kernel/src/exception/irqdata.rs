use core::{any::Any, fmt::Debug};

use alloc::sync::{Arc, Weak};

use crate::libs::spinlock::SpinLock;

use super::{
    irqchip::{IrqChip, IrqChipData},
    irqdomain::IrqDomain,
    msi::MsiDesc,
    HardwareIrqNumber, IrqNumber,
};

/// per irq chip data passed down to chip functions
///
/// 该结构体用于表示每个Irq的私有数据，且与具体的中断芯片绑定
///
/// 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/irq.h#179
#[allow(dead_code)]
#[derive(Debug)]
pub struct IrqData {
    /// 中断号, 用于表示软件逻辑视角的中断号，全局唯一
    irq: IrqNumber,
    inner: SpinLock<InnerIrqData>,
}

impl IrqData {
    pub fn new(
        irq: IrqNumber,
        hwirq: HardwareIrqNumber,
        common_data: Arc<IrqCommonData>,
        chip: Arc<dyn IrqChip>,
    ) -> Self {
        return IrqData {
            irq,
            inner: SpinLock::new(InnerIrqData {
                hwirq,
                common_data,
                chip,
                chip_data: None,
                domain: None,
                parent_data: None,
            }),
        };
    }

    pub fn irqd_set(&self, status: IrqStatus) {
        // clone是为了释放inner锁
        let common_data = self.inner.lock().common_data.clone();
        common_data.irqd_set(status);
    }

    #[allow(dead_code)]
    pub fn irqd_clear(&self, status: IrqStatus) {
        // clone是为了释放inner锁
        let common_data = self.inner.lock().common_data.clone();
        common_data.irqd_clear(status);
    }

    pub fn irq(&self) -> IrqNumber {
        self.irq
    }
}

#[allow(dead_code)]
#[derive(Debug)]
struct InnerIrqData {
    /// 硬件中断号, 用于表示在某个IrqDomain中的中断号
    hwirq: HardwareIrqNumber,
    /// 涉及的所有irqchip之间共享的数据
    common_data: Arc<IrqCommonData>,
    /// 绑定到的中断芯片
    chip: Arc<dyn IrqChip>,
    /// 中断芯片的私有数据（与当前irq相关）
    chip_data: Option<Arc<dyn IrqChipData>>,
    /// 中断域
    domain: Option<Arc<IrqDomain>>,
    /// 中断的父中断（如果具有中断域继承的话）
    parent_data: Option<Weak<IrqData>>,
}

/// per irq data shared by all irqchips
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/irq.h#147
#[derive(Debug)]
pub struct IrqCommonData {
    inner: SpinLock<InnerIrqCommonData>,
}

impl IrqCommonData {
    pub fn new() -> Self {
        let inner = InnerIrqCommonData {
            state: IrqStatus::empty(),
            handler_data: None,
            msi_desc: None,
        };
        return IrqCommonData {
            inner: SpinLock::new(inner),
        };
    }

    pub fn irqd_set(&self, status: IrqStatus) {
        self.inner.lock_irqsave().irqd_set(status);
    }

    pub fn irqd_clear(&self, status: IrqStatus) {
        self.inner.lock_irqsave().irqd_clear(status);
    }
}

#[allow(dead_code)]
#[derive(Debug)]
struct InnerIrqCommonData {
    /// status information for irq chip functions.
    state: IrqStatus,
    /// per-IRQ data for the irq_chip methods
    handler_data: Option<Arc<dyn IrqHandlerData>>,
    msi_desc: Option<Arc<MsiDesc>>,
    // todo: affinity
}

impl InnerIrqCommonData {
    pub fn irqd_set(&mut self, status: IrqStatus) {
        self.state.insert(status);
    }

    pub fn irqd_clear(&mut self, status: IrqStatus) {
        self.state.remove(status);
    }
}

pub trait IrqHandlerData: Send + Sync + Any + Debug {}

bitflags! {
    /// 中断线状态
    /// https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/irq.h?fi=IRQ_TYPE_PROBE#77
    pub struct IrqLineStatus: u32 {
        /// 默认，未指明类型
        const IRQ_TYPE_NONE     = 0x00000000;
        /// 上升沿触发
        const IRQ_TYPE_EDGE_RISING  = 0x00000001;
        /// 下降沿触发
        const IRQ_TYPE_EDGE_FALLING = 0x00000002;
        /// 上升沿和下降沿触发
        const IRQ_TYPE_EDGE_BOTH    = Self::IRQ_TYPE_EDGE_RISING.bits | Self::IRQ_TYPE_EDGE_FALLING.bits;
        /// 高电平触发
        const IRQ_TYPE_LEVEL_HIGH   = 0x00000004;
        /// 低电平触发
        const IRQ_TYPE_LEVEL_LOW    = 0x00000008;
        /// 过滤掉电平位的掩码
        const IRQ_TYPE_LEVEL_MASK   = Self::IRQ_TYPE_LEVEL_LOW.bits | Self::IRQ_TYPE_LEVEL_HIGH.bits;
        /// 上述位掩码的掩码
        const IRQ_TYPE_SENSE_MASK   = 0x0000000f;
        /// 某些PICs使用此类型要求 `IrqChip::irq_set_type()` 设置硬件到一个合理的默认值
        /// （由irqdomain的map()回调使用，以便为新分配的描述符同步硬件状态和软件标志位）。
        const IRQ_TYPE_DEFAULT      = Self::IRQ_TYPE_SENSE_MASK.bits;

        /// 特定于探测的过程中的特殊标志
        const IRQ_TYPE_PROBE        = 0x00000010;

        /// 中断是电平类型。当上述触发位通过`IrqChip::irq_set_type()` 修改时，也会在代码中更新
        const IRQ_LEVEL     = 1 << 8;
        /// 标记一个PER_CPU的中断。将保护其免受亲和性设置的影响
        const IRQ_PER_CPU       = 1 << 9;
        /// 中断不能被自动探测
        const IRQ_NOPROBE       = 1 << 10;
        /// 中断不能通过request_irq()请求
        const IRQ_NOREQUEST     = 1 << 11;
        /// 中断在request/setup_irq()中不会自动启用
        const IRQ_NOAUTOEN      = 1 << 12;
        /// 中断不能被平衡（亲和性设置）
        const IRQ_NO_BALANCING      = 1 << 13;
        /// 中断可以从进程上下文中迁移
        const IRQ_MOVE_PCNTXT       = 1 << 14;
        /// 中断嵌套在另一个线程中
        const IRQ_NESTED_THREAD = 1 << 15;
        /// 中断不能被线程化
        const IRQ_NOTHREAD      = 1 << 16;
        /// Dev_id是一个per-CPU变量
        const IRQ_PER_CPU_DEVID = 1 << 17;
        /// 总是由另一个中断轮询。将其从错误的中断检测机制和核心侧轮询中排除
        const IRQ_IS_POLLED     = 1 << 18;
        /// 禁用延迟的中断禁用 (Disable lazy irq disable)
        const IRQ_DISABLE_UNLAZY    = 1 << 19;
        /// 在/proc/interrupts中不显示
        const IRQ_HIDDEN        = 1 << 20;
        /// 从note_interrupt()调试中排除
        const IRQ_NO_DEBUG      = 1 << 21;
    }



}
bitflags! {
    /// 中断状态（存储在IrqCommonData)
    ///
    /// 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/irq.h#227
    pub struct IrqStatus: u32 {
        /// 触发类型位的掩码
        const IRQD_TRIGGER_MASK = 0xf;
        /// 亲和性设置待处理
        const IRQD_SETAFFINITY_PENDING = 1 << 8;
        /// 中断已激活
        const IRQD_ACTIVATED = 1 << 9;
        /// 对此IRQ禁用平衡
        const IRQD_NO_BALANCING = 1 << 10;
        /// 中断是每个CPU特定的
        const IRQD_PER_CPU = 1 << 11;
        /// 中断亲和性已设置
        const IRQD_AFFINITY_SET = 1 << 12;
        /// 中断是电平触发
        const IRQD_LEVEL = 1 << 13;
        /// 中断配置为从挂起状态唤醒
        const IRQD_WAKEUP_STATE = 1 << 14;
        /// 中断可以在进程上下文中移动
        const IRQD_MOVE_PCNTXT = 1 << 15;
        /// 中断被禁用
        const IRQD_IRQ_DISABLED = 1 << 16;
        /// 中断被屏蔽
        const IRQD_IRQ_MASKED = 1 << 17;
        /// 中断正在处理中
        const IRQD_IRQ_INPROGRESS = 1 << 18;
        /// 唤醒模式已准备就绪
        const IRQD_WAKEUP_ARMED = 1 << 19;
        /// 中断被转发到一个虚拟CPU
        const IRQD_FORWARDED_TO_VCPU = 1 << 20;
        /// 亲和性由内核自动管理
        const IRQD_AFFINITY_MANAGED = 1 << 21;
        /// 中断已启动
        const IRQD_IRQ_STARTED = 1 << 22;
        /// 由于空亲和性掩码而关闭的中断。仅适用于亲和性管理的中断。
        const IRQD_MANAGED_SHUTDOWN = 1 << 23;
        /// IRQ只允许单个亲和性目标
        const IRQD_SINGLE_TARGET = 1 << 24;
        /// 预期的触发器已设置
        const IRQD_DEFAULT_TRIGGER_SET = 1 << 25;
        /// 可以使用保留模式
        const IRQD_CAN_RESERVE = 1 << 26;
        /// Non-maskable MSI quirk for affinity change required
        const IRQD_MSI_NOMASK_QUIRK = 1 << 27;
        /// 强制要求`handle_irq_()`只能在真实的中断上下文中调用
        const IRQD_HANDLE_ENFORCE_IRQCTX = 1 << 28;
        /// 激活时设置亲和性。在禁用时不要调用irq_chip::irq_set_affinity()。
        const IRQD_AFFINITY_ON_ACTIVATE = 1 << 29;
        /// 如果irqpm具有标志 IRQCHIP_ENABLE_WAKEUP_ON_SUSPEND，则在挂起时中断被启用。
        const IRQD_IRQ_ENABLED_ON_SUSPEND = 1 << 30;
    }
}

#[allow(dead_code)]
impl IrqStatus {
    pub const fn is_set_affinity_pending(&self) -> bool {
        self.contains(Self::IRQD_SETAFFINITY_PENDING)
    }

    pub const fn is_per_cpu(&self) -> bool {
        self.contains(Self::IRQD_PER_CPU)
    }

    pub const fn can_balance(&self) -> bool {
        !((self.bits & (Self::IRQD_PER_CPU.bits | Self::IRQD_NO_BALANCING.bits)) != 0)
    }

    pub const fn affinity_was_set(&self) -> bool {
        self.contains(Self::IRQD_AFFINITY_SET)
    }

    pub fn mark_affinity_set(&mut self) {
        self.insert(Self::IRQD_AFFINITY_SET);
    }

    pub const fn trigger_type_was_set(&self) -> bool {
        self.contains(Self::IRQD_DEFAULT_TRIGGER_SET)
    }

    pub fn mark_trigger_type_set(&mut self) {
        self.insert(Self::IRQD_DEFAULT_TRIGGER_SET);
    }

    pub const fn trigger_type(&self) -> IrqLineStatus {
        IrqLineStatus::from_bits_truncate(self.bits & Self::IRQD_TRIGGER_MASK.bits)
    }

    /// Must only be called inside irq_chip.irq_set_type() functions or
    /// from the DT/ACPI setup code.
    pub const fn set_trigger_type(&mut self, trigger: IrqLineStatus) {
        self.bits &= !Self::IRQD_TRIGGER_MASK.bits;
        self.bits |= trigger.bits & Self::IRQD_TRIGGER_MASK.bits;

        self.bits |= Self::IRQD_DEFAULT_TRIGGER_SET.bits;
    }

    pub const fn is_level_type(&self) -> bool {
        self.contains(Self::IRQD_LEVEL)
    }

    /// Must only be called of irqchip.irq_set_affinity() or low level
    /// hierarchy domain allocation functions.
    pub fn set_single_target(&mut self) {
        self.insert(Self::IRQD_SINGLE_TARGET);
    }

    pub const fn is_single_target(&self) -> bool {
        self.contains(Self::IRQD_SINGLE_TARGET)
    }

    pub fn set_handle_enforce_irqctx(&mut self) {
        self.insert(Self::IRQD_HANDLE_ENFORCE_IRQCTX);
    }

    pub const fn is_handle_enforce_irqctx(&self) -> bool {
        self.contains(Self::IRQD_HANDLE_ENFORCE_IRQCTX)
    }

    pub const fn is_enabled_on_suspend(&self) -> bool {
        self.contains(Self::IRQD_IRQ_ENABLED_ON_SUSPEND)
    }

    pub const fn is_wakeup_set(&self) -> bool {
        self.contains(Self::IRQD_WAKEUP_STATE)
    }

    pub const fn can_move_in_process_context(&self) -> bool {
        self.contains(Self::IRQD_MOVE_PCNTXT)
    }

    pub const fn is_irq_disabled(&self) -> bool {
        self.contains(Self::IRQD_IRQ_DISABLED)
    }

    pub const fn is_irq_masked(&self) -> bool {
        self.contains(Self::IRQD_IRQ_MASKED)
    }

    pub const fn is_irq_in_progress(&self) -> bool {
        self.contains(Self::IRQD_IRQ_INPROGRESS)
    }

    pub const fn is_wakeup_armed(&self) -> bool {
        self.contains(Self::IRQD_WAKEUP_ARMED)
    }

    pub const fn is_forwarded_to_vcpu(&self) -> bool {
        self.contains(Self::IRQD_FORWARDED_TO_VCPU)
    }

    pub fn set_forwarded_to_vcpu(&mut self) {
        self.insert(Self::IRQD_FORWARDED_TO_VCPU);
    }

    pub const fn is_affinity_managed(&self) -> bool {
        self.contains(Self::IRQD_AFFINITY_MANAGED)
    }

    pub const fn is_activated(&self) -> bool {
        self.contains(Self::IRQD_ACTIVATED)
    }

    pub fn set_activated(&mut self) {
        self.insert(Self::IRQD_ACTIVATED);
    }

    pub fn clear_activated(&mut self) {
        self.remove(Self::IRQD_ACTIVATED);
    }

    pub const fn is_started(&self) -> bool {
        self.contains(Self::IRQD_IRQ_STARTED)
    }

    pub const fn is_managed_and_shutdown(&self) -> bool {
        self.contains(Self::IRQD_MANAGED_SHUTDOWN)
    }

    pub fn set_can_reserve(&mut self) {
        self.insert(Self::IRQD_CAN_RESERVE);
    }

    pub const fn can_reserve(&self) -> bool {
        self.contains(Self::IRQD_CAN_RESERVE)
    }

    pub fn clear_can_reserve(&mut self) {
        self.remove(Self::IRQD_CAN_RESERVE);
    }

    pub fn set_msi_nomask_quirk(&mut self) {
        self.insert(Self::IRQD_MSI_NOMASK_QUIRK);
    }

    pub fn clear_msi_nomask_quirk(&mut self) {
        self.remove(Self::IRQD_MSI_NOMASK_QUIRK);
    }

    pub const fn is_msi_nomask_quirk(&self) -> bool {
        self.contains(Self::IRQD_MSI_NOMASK_QUIRK)
    }

    pub fn set_affinity_on_activate(&mut self) {
        self.insert(Self::IRQD_AFFINITY_ON_ACTIVATE);
    }

    pub const fn is_affinity_on_activate(&self) -> bool {
        self.contains(Self::IRQD_AFFINITY_ON_ACTIVATE)
    }
}
