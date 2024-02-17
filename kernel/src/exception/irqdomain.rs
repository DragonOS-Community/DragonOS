use core::fmt::Debug;

use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;
use system_error::SystemError;

use crate::{
    driver::{base::device::Device, open_firmware::device_node::DeviceNode},
    libs::{rwlock::RwLock, spinlock::SpinLock},
};

use super::{
    irqchip::{IrqChipGeneric, IrqGcFlags},
    HardwareIrqNumber, IrqNumber,
};

/// 中断域
///
/// 用于把硬件中断号翻译为软件中断号的映射的对象
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/irqdomain.h#164
#[allow(dead_code)]
#[derive(Debug)]
pub struct IrqDomain {
    /// 中断域的名字 (二选一)
    name: Option<&'static str>,
    allocated_name: Option<String>,
    /// 中断域的操作
    ops: &'static dyn IrqDomainOps,
    inner: SpinLock<InnerIrqDomain>,
    /// 中断号反向映射
    revmap: RwLock<IrqDomainRevMap>,
}

#[allow(dead_code)]
#[derive(Debug)]
struct InnerIrqDomain {
    /// host per irq_domain flags
    flags: IrqDomainFlags,
    /// The number of mapped interrupts
    mapcount: u32,
    bus_token: IrqDomainBusToken,
    /// 指向 generic chip 列表的指针。
    /// 有一个辅助函数用于为中断控制器驱动程序设置一个或
    /// 多个 generic chip，该函数使用此指针并依赖于 generic chip 库。
    generic_chip: Option<Arc<IrqDomainChipGeneric>>,
    /// Pointer to a device that the domain represent, and that will be
    /// used for power management purposes.
    device: Option<Arc<dyn Device>>,
    /// Pointer to parent irq_domain to support hierarchy irq_domains
    parent: Option<Weak<IrqDomain>>,
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/irqdomain.h#190
#[allow(dead_code)]
#[derive(Debug)]
struct IrqDomainRevMap {
    map: HashMap<HardwareIrqNumber, IrqNumber>,
    hwirq_max: HardwareIrqNumber,
}

bitflags! {
    pub struct IrqDomainFlags: u32 {
        /// Irq domain is hierarchical
        const HIERARCHY = (1 << 0);
        /// Irq domain name was allocated dynamically
        const NAME_ALLOCATED = (1 << 1);
        /// Irq domain is an IPI domain with virq per cpu
        const IPI_PER_CPU = (1 << 2);
        /// Irq domain is an IPI domain with single virq
        const IPI_SINGLE = (1 << 3);
        /// Irq domain implements MSIs
        const MSI = (1 << 4);
        /// Irq domain implements MSI remapping
        const MSI_REMAP = (1 << 5);
        /// Quirk to handle MSI implementations which do not provide masking
        const MSI_NOMASK_QUIRK = (1 << 6);
        /// Irq domain doesn't translate anything
        const NO_MAP = (1 << 7);
        /// Flags starting from IRQ_DOMAIN_FLAG_NONCORE are reserved
        /// for implementation specific purposes and ignored by the core code
        const NONCORE = (1 << 16);
    }
}

/// 如果多个域有相同的设备节点，但服务于不同的目的（例如，一个域用于PCI/MSI，另一个用于有线IRQs），
/// 它们可以使用特定于总线的token进行区分。预计大多数域只会携带`DomainBusAny`。
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/irqdomain.h#78
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqDomainBusToken {
    Any = 0,
    Wired,
    GenericMsi,
    PciMsi,
    PlatformMsi,
    Nexus,
    Ipi,
    FslMcMsi,
    TiSciIntaMsi,
    Wakeup,
    VmdMsi,
}

/// IrqDomain的操作方法
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/irqdomain.h#107
pub trait IrqDomainOps: Debug {
    /// 匹配一个中断控制器设备节点到一个主机。
    fn match_node(
        &self,
        irq_domain: &Arc<IrqDomain>,
        device_node: &Arc<DeviceNode>,
        bus_token: IrqDomainBusToken,
    ) -> bool;

    /// 创建或更新一个虚拟中断号与一个硬件中断号之间的映射。
    /// 对于给定的映射，这只会被调用一次。
    fn map(
        &self,
        irq_domain: &Arc<IrqDomain>,
        hwirq: HardwareIrqNumber,
        virq: IrqNumber,
    ) -> Result<(), SystemError>;

    /// 删除一个虚拟中断号与一个硬件中断号之间的映射。
    fn unmap(&self, irq_domain: &Arc<IrqDomain>, virq: IrqNumber);
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct IrqDomainChipGeneric {
    inner: SpinLock<InnerIrqDomainChipGeneric>,
}

#[allow(dead_code)]
#[derive(Debug)]
struct InnerIrqDomainChipGeneric {
    irqs_per_chip: u32,
    flags_to_clear: IrqGcFlags,
    flags_to_set: IrqGcFlags,
    gc_flags: IrqGcFlags,
    gc: Vec<Arc<IrqChipGeneric>>,
}
