use core::fmt::Debug;

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;
use system_error::SystemError;

use crate::{
    driver::{base::device::Device, open_firmware::device_node::DeviceNode},
    exception::{irqdata::IrqLineStatus, irqdesc::irq_desc_manager, manage::irq_manager},
    libs::{rwlock::RwLock, spinlock::SpinLock},
};

use super::{
    dummychip::no_irq_chip,
    irqchip::{IrqChip, IrqChipData, IrqChipGeneric, IrqGcFlags},
    irqdata::{IrqData, IrqHandlerData},
    irqdesc::IrqFlowHandler,
    HardwareIrqNumber, IrqNumber,
};

static mut IRQ_DOMAIN_MANAGER: Option<Arc<IrqDomainManager>> = None;

/// 获取中断域管理器的引用
#[inline(always)]
pub fn irq_domain_manager() -> &'static Arc<IrqDomainManager> {
    unsafe { IRQ_DOMAIN_MANAGER.as_ref().unwrap() }
}

pub(super) fn irq_domain_manager_init() {
    unsafe {
        IRQ_DOMAIN_MANAGER = Some(Arc::new(IrqDomainManager::new()));
    }
}
/// 中断域管理器
pub struct IrqDomainManager {
    domains: SpinLock<Vec<Arc<IrqDomain>>>,
    inner: RwLock<InnerIrqDomainManager>,
}

impl IrqDomainManager {
    pub fn new() -> IrqDomainManager {
        IrqDomainManager {
            domains: SpinLock::new(Vec::new()),
            inner: RwLock::new(InnerIrqDomainManager {
                default_domain: None,
            }),
        }
    }

    /// 创建一个新的线性映射的irqdomain, 并将其添加到irqdomain管理器中
    ///
    /// 创建的irqdomain，中断号是线性的，即从0开始，依次递增
    ///
    /// ## 参数
    ///
    /// - `name` - 中断域的名字
    /// - `ops` - 中断域的操作
    /// - `irq_size` - 中断号的数量
    #[allow(dead_code)]
    pub fn create_and_add_linear(
        &self,
        name: String,
        ops: &'static dyn IrqDomainOps,
        irq_size: u32,
    ) -> Option<Arc<IrqDomain>> {
        self.create_and_add(
            name,
            ops,
            IrqNumber::new(0),
            HardwareIrqNumber::new(0),
            irq_size,
        )
    }

    /// 创建一个新的irqdomain, 并将其添加到irqdomain管理器中
    ///
    /// ## 参数
    ///
    /// - `name` - 中断域的名字
    /// - `ops` - 中断域的操作
    /// - `first_irq` - 起始软件中断号
    /// - `first_hwirq` - 起始硬件中断号
    /// - `irq_size` - 中断号的数量
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/irqdomain.c?fi=__irq_domain_add#139
    pub fn create_and_add(
        &self,
        name: String,
        ops: &'static dyn IrqDomainOps,
        first_irq: IrqNumber,
        first_hwirq: HardwareIrqNumber,
        irq_size: u32,
    ) -> Option<Arc<IrqDomain>> {
        let domain = IrqDomain::new(
            None,
            Some(name),
            ops,
            IrqDomainFlags::NAME_ALLOCATED,
            IrqDomainBusToken::Any,
            first_irq + irq_size,
            first_hwirq + irq_size,
        )?;

        self.add_domain(domain.clone());

        self.domain_associate_many(&domain, first_irq, first_hwirq, irq_size);

        return Some(domain);
    }

    fn add_domain(&self, domain: Arc<IrqDomain>) {
        self.domains.lock_irqsave().push(domain);
    }

    #[allow(dead_code)]
    pub fn remove_domain(&self, domain: &Arc<IrqDomain>) {
        let mut domains = self.domains.lock_irqsave();
        let index = domains
            .iter()
            .position(|x| Arc::ptr_eq(x, domain))
            .expect("domain not found");
        domains.remove(index);
    }

    /// 获取默认的中断域
    #[allow(dead_code)]
    pub fn default_domain(&self) -> Option<Arc<IrqDomain>> {
        self.inner.read().default_domain.clone()
    }

    /// 设置默认的中断域
    ///
    /// 在创建IRQ映射的时候，如果没有指定中断域，就会使用默认的中断域
    pub fn set_default_domain(&self, domain: Arc<IrqDomain>) {
        self.inner.write_irqsave().default_domain = Some(domain);
    }

    /// 将指定范围的硬件中断号与软件中断号一一对应的关联起来
    ///
    /// ## 参数
    ///
    /// - `domain` - 中断域
    /// - `first_irq` - 起始软件中断号
    /// - `first_hwirq` - 起始硬件中断号
    /// - `count` - 数量
    pub fn domain_associate_many(
        &self,
        domain: &Arc<IrqDomain>,
        first_irq: IrqNumber,
        first_hwirq: HardwareIrqNumber,
        count: u32,
    ) {
        for i in 0..count {
            if let Err(e) = self.domain_associate(domain, first_irq + i, first_hwirq + i) {
                kwarn!("domain associate failed: {:?}, domain '{:?}' didn't like hwirq {} to virq {} mapping.", e, domain.name(), (first_hwirq + i).data(), (first_irq + i).data());
            }
        }
    }

    /// 将一个硬件中断号与一个软件中断号关联起来
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/irqdomain.c#562
    pub fn domain_associate(
        &self,
        domain: &Arc<IrqDomain>,
        irq: IrqNumber,
        hwirq: HardwareIrqNumber,
    ) -> Result<(), SystemError> {
        if hwirq >= domain.revmap.read_irqsave().hwirq_max {
            kwarn!(
                "hwirq {} is out of range for domain {:?}",
                hwirq.data(),
                domain.name()
            );
            return Err(SystemError::EINVAL);
        }
        let irq_data = irq_desc_manager()
            .lookup(irq)
            .ok_or_else(|| {
                kwarn!("irq_desc not found for irq {}", irq.data());
                SystemError::EINVAL
            })?
            .irq_data();
        if irq_data.domain().is_some() {
            kwarn!(
                "irq {} is already associated with domain {:?}",
                irq.data(),
                irq_data.domain().unwrap().name()
            );
            return Err(SystemError::EINVAL);
        }

        let mut irq_data_guard = irq_data.inner();
        irq_data_guard.set_hwirq(hwirq);
        irq_data_guard.set_domain(Some(domain.clone()));
        drop(irq_data_guard);
        let r = domain.ops.map(&domain, hwirq, irq);
        if let Err(e) = r {
            if e != SystemError::ENOSYS {
                if e != SystemError::EPERM {
                    kinfo!("domain associate failed: {:?}, domain '{:?}' didn't like hwirq {} to virq {} mapping.", e, domain.name(), hwirq.data(), irq.data());
                }
                let mut irq_data_guard = irq_data.inner();
                irq_data_guard.set_domain(None);
                irq_data_guard.set_hwirq(HardwareIrqNumber::new(0));
                return Err(e);
            }
        }

        if domain.name().is_none() {
            let chip = irq_data.chip_info_read_irqsave().chip();
            domain.set_name(chip.name().to_string());
        }

        self.irq_domain_set_mapping(&domain, hwirq, irq_data);

        irq_manager().irq_clear_status_flags(irq, IrqLineStatus::IRQ_NOREQUEST)?;

        return Ok(());
    }

    fn irq_domain_set_mapping(
        &self,
        domain: &Arc<IrqDomain>,
        hwirq: HardwareIrqNumber,
        irq_data: Arc<IrqData>,
    ) {
        if domain.no_map() {
            return;
        }

        domain.revmap.write_irqsave().insert(hwirq, irq_data);
    }
    /// 递归调用 domain_ops->activate 以激活中断
    ///
    /// ## 参数
    ///
    /// - irq_data: 与中断关联的最外层 irq_data
    /// - reserve: 如果为true，则仅预留一个中断向量，而不是分配一个
    ///
    /// 这是调用 domain_ops->activate 以编程中断控制器的第二步，以便中断实际上可以被传递。
    pub fn activate_irq(&self, irq_data: &Arc<IrqData>, reserve: bool) -> Result<(), SystemError> {
        let mut r = Ok(());
        if !irq_data.common_data().status().is_activated() {
            r = self.do_activate_irq(Some(irq_data.clone()), reserve);
        }

        if !r.is_ok() {
            irq_data.common_data().status().set_activated();
        }

        return r;
    }

    #[inline(never)]
    fn do_activate_irq(
        &self,
        irq_data: Option<Arc<IrqData>>,
        reserve: bool,
    ) -> Result<(), SystemError> {
        let mut r = Ok(());

        if irq_data.is_some() && irq_data.as_ref().unwrap().domain().is_some() {
            let domain = irq_data.as_ref().unwrap().domain().unwrap();

            let irq_data = irq_data.unwrap();

            let parent_data = irq_data.parent_data().map(|x| x.upgrade()).flatten();
            if let Some(parent_data) = parent_data.clone() {
                r = self.do_activate_irq(Some(parent_data), reserve);
            }

            if r.is_err() {
                let tmpr = domain.ops.activate(&domain, &irq_data, reserve);
                if let Err(e) = tmpr {
                    if e != SystemError::ENOSYS && parent_data.is_some() {
                        self.do_deactivate_irq(parent_data);
                    }
                }
            }
        }

        return r;
    }

    fn do_deactivate_irq(&self, irq_data: Option<Arc<IrqData>>) {
        if let Some(irq_data) = irq_data {
            if let Some(domain) = irq_data.domain() {
                domain.ops.deactivate(&domain, &irq_data);
                let pp = irq_data.parent_data().map(|x| x.upgrade()).flatten();

                if pp.is_some() {
                    self.do_deactivate_irq(pp);
                }
            }
        }
    }

    /// `irq_domain_set_info` - 在 @domain 中为 @virq 设置完整的数据
    ///
    /// ## 参数
    ///
    /// - `domain`: 要匹配的中断域
    /// - `virq`: IRQ号
    /// - `hwirq`: 硬件中断号
    /// - `chip`: 相关的中断芯片
    /// - `chip_data`: 相关的中断芯片数据
    /// - `handler`: 中断流处理器
    /// - `handler_data`: 中断流处理程序数据
    /// - `handler_name`: 中断处理程序名称
    pub fn domain_set_info(
        &self,
        domain: &Arc<IrqDomain>,
        virq: IrqNumber,
        hwirq: HardwareIrqNumber,
        chip: Arc<dyn IrqChip>,
        chip_data: Option<Arc<dyn IrqChipData>>,
        flow_handler: &'static dyn IrqFlowHandler,
        handler_data: Option<Arc<dyn IrqHandlerData>>,
        handler_name: Option<String>,
    ) {
        let r = self.domain_set_hwirq_and_chip(domain, virq, hwirq, Some(chip), chip_data);
        if r.is_err() {
            return;
        }
        irq_manager().__irq_set_handler(virq, flow_handler, false, handler_name);
        irq_manager().irq_set_handler_data(virq, handler_data).ok();
    }

    /// `domain_set_hwirq_and_chip` - 在 @domain 中为 @virq 设置 hwirq 和 irqchip
    ///
    /// ## 参数
    ///
    /// - `domain`: 要匹配的中断域
    /// - `virq`: IRQ号
    /// - `hwirq`: hwirq号
    /// - `chip`: 相关的中断芯片
    /// - `chip_data`: 相关的芯片数据
    pub fn domain_set_hwirq_and_chip(
        &self,
        domain: &Arc<IrqDomain>,
        virq: IrqNumber,
        hwirq: HardwareIrqNumber,
        chip: Option<Arc<dyn IrqChip>>,
        chip_data: Option<Arc<dyn IrqChipData>>,
    ) -> Result<(), SystemError> {
        let irq_data: Arc<IrqData> = self
            .domain_get_irq_data(domain, virq)
            .ok_or(SystemError::ENOENT)?;
        let mut inner = irq_data.inner();
        let mut chip_info = irq_data.chip_info_write_irqsave();

        inner.set_hwirq(hwirq);
        if let Some(chip) = chip {
            chip_info.set_chip(Some(chip));
        } else {
            chip_info.set_chip(Some(no_irq_chip()));
        };

        chip_info.set_chip_data(chip_data);

        return Ok(());
    }

    /// `irq_domain_get_irq_data` - 获取与 @virq 和 @domain 关联的 irq_data
    ///
    /// ## 参数
    ///
    /// - `domain`: 要匹配的域
    /// - `virq`: 要获取 irq_data 的IRQ号
    pub fn domain_get_irq_data(
        &self,
        domain: &Arc<IrqDomain>,
        virq: IrqNumber,
    ) -> Option<Arc<IrqData>> {
        let desc = irq_desc_manager().lookup(virq)?;
        let mut irq_data = Some(desc.irq_data());

        while irq_data.is_some() {
            let dt = irq_data.unwrap();
            if dt.domain().is_some() && Arc::ptr_eq(dt.domain().as_ref().unwrap(), domain) {
                return Some(dt);
            }
            irq_data = dt.parent_data().map(|x| x.upgrade()).flatten();
        }

        return None;
    }
}

struct InnerIrqDomainManager {
    default_domain: Option<Arc<IrqDomain>>,
}

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
    allocated_name: SpinLock<Option<String>>,
    /// 中断域的操作
    ops: &'static dyn IrqDomainOps,
    inner: SpinLock<InnerIrqDomain>,
    /// 中断号反向映射
    revmap: RwLock<IrqDomainRevMap>,
}

#[allow(dead_code)]
#[derive(Debug)]
struct InnerIrqDomain {
    /// this field not touched by the core code
    host_data: Option<Arc<dyn IrqChipData>>,
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

impl IrqDomain {
    #[allow(dead_code)]
    pub fn new(
        name: Option<&'static str>,
        allocated_name: Option<String>,
        ops: &'static dyn IrqDomainOps,
        flags: IrqDomainFlags,
        bus_token: IrqDomainBusToken,
        irq_max: IrqNumber,
        hwirq_max: HardwareIrqNumber,
    ) -> Option<Arc<Self>> {
        if name.is_none() && allocated_name.is_none() {
            return None;
        }

        let x = IrqDomain {
            name,
            allocated_name: SpinLock::new(allocated_name),
            ops,
            inner: SpinLock::new(InnerIrqDomain {
                host_data: None,
                flags,
                mapcount: 0,
                bus_token,
                generic_chip: None,
                device: None,
                parent: None,
            }),
            revmap: RwLock::new(IrqDomainRevMap {
                map: HashMap::new(),
                hwirq_max,
                irq_max,
            }),
        };

        return Some(Arc::new(x));
    }

    /// 中断域是否不对中断号进行转换
    pub fn no_map(&self) -> bool {
        self.inner
            .lock_irqsave()
            .flags
            .contains(IrqDomainFlags::NO_MAP)
    }

    #[allow(dead_code)]
    fn set_hwirq_max(&self, hwirq_max: HardwareIrqNumber) {
        self.revmap.write_irqsave().hwirq_max = hwirq_max;
    }

    pub fn name(&self) -> Option<String> {
        if let Some(name) = self.name {
            return Some(name.to_string());
        }
        return self.allocated_name.lock_irqsave().clone();
    }

    pub fn set_name(&self, name: String) {
        *self.allocated_name.lock_irqsave() = Some(name);
    }

    /// The number of mapped interrupts
    pub fn map_count(&self) -> u32 {
        self.revmap.read().map.len() as u32
    }

    pub fn host_data(&self) -> Option<Arc<dyn IrqChipData>> {
        self.inner.lock_irqsave().host_data.clone()
    }

    pub fn set_host_data(&self, host_data: Option<Arc<dyn IrqChipData>>) {
        self.inner.lock_irqsave().host_data = host_data;
    }
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/irqdomain.h#190
#[allow(dead_code)]
#[derive(Debug)]
struct IrqDomainRevMap {
    map: HashMap<HardwareIrqNumber, Arc<IrqData>>,
    hwirq_max: HardwareIrqNumber,
    irq_max: IrqNumber,
}

impl IrqDomainRevMap {
    fn insert(&mut self, hwirq: HardwareIrqNumber, irq_data: Arc<IrqData>) {
        self.map.insert(hwirq, irq_data);
    }

    #[allow(dead_code)]
    fn remove(&mut self, hwirq: HardwareIrqNumber) {
        self.map.remove(&hwirq);
    }

    #[allow(dead_code)]
    fn lookup(&self, hwirq: HardwareIrqNumber) -> Option<Arc<IrqData>> {
        self.map.get(&hwirq).cloned()
    }
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
pub trait IrqDomainOps: Debug + Send + Sync {
    /// 匹配一个中断控制器设备节点到一个主机。
    fn match_node(
        &self,
        _irq_domain: &Arc<IrqDomain>,
        _device_node: &Arc<DeviceNode>,
        _bus_token: IrqDomainBusToken,
    ) -> bool {
        false
    }

    /// 创建或更新一个虚拟中断号与一个硬件中断号之间的映射。
    /// 对于给定的映射，这只会被调用一次。
    ///
    /// 如果没有实现这个方法，那么就会返回`ENOSYS`
    fn map(
        &self,
        _irq_domain: &Arc<IrqDomain>,
        _hwirq: HardwareIrqNumber,
        _virq: IrqNumber,
    ) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// 删除一个虚拟中断号与一个硬件中断号之间的映射。
    fn unmap(&self, irq_domain: &Arc<IrqDomain>, virq: IrqNumber);

    fn activate(
        &self,
        _domain: &Arc<IrqDomain>,
        _irq_data: &Arc<IrqData>,
        _reserve: bool,
    ) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    fn deactivate(&self, _domain: &Arc<IrqDomain>, _irq_data: &Arc<IrqData>) {}
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
