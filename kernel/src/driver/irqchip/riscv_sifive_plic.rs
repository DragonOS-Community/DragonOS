//! 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/drivers/irqchip/irq-sifive-plic.c
//!
//!
//!
//!  This driver implements a version of the RISC-V PLIC with the actual layout
//!  specified in chapter 8 of the SiFive U5 Coreplex Series Manual:
//!
//!      https://static.dev.sifive.com/U54-MC-RVCoreIP.pdf
//!
//!  The largest number supported by devices marked as 'sifive,plic-1.0.0', is
//!  1024, of which device 0 is defined as non-existent by the RISC-V Privileged
//!  Spec.
//!

use core::{
    fmt::Debug,
    ops::Deref,
    ptr::{read_volatile, write_volatile},
    sync::atomic::AtomicBool,
};

use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};
use bitmap::AllocBitmap;
use fdt::node::FdtNode;
use log::{debug, warn};
use system_error::SystemError;

use crate::{
    arch::interrupt::TrapFrame,
    driver::open_firmware::fdt::open_firmware_fdt_driver,
    exception::{
        handle::fast_eoi_irq_handler,
        irqchip::{IrqChip, IrqChipData, IrqChipFlags, IrqChipSetMaskResult},
        irqdata::IrqData,
        irqdesc::{irq_desc_manager, GenericIrqHandler},
        irqdomain::{irq_domain_manager, IrqDomain, IrqDomainOps},
        manage::irq_manager,
        HardwareIrqNumber, IrqNumber,
    },
    libs::{
        cpumask::CpuMask,
        once::Once,
        spinlock::{SpinLock, SpinLockGuard},
    },
    mm::{
        mmio_buddy::{mmio_pool, MMIOSpaceGuard},
        percpu::{PerCpu, PerCpuVar},
        PhysAddr, VirtAddr,
    },
    smp::cpu::{smp_cpu_manager, ProcessorId},
};

static mut PLIC_HANDLERS: Option<PerCpuVar<PlicHandler>> = None;

static mut PLIC_IRQ_CHIP: Option<Arc<PlicIrqChip>> = None;

#[inline(always)]
fn plic_irq_chip() -> Arc<PlicIrqChip> {
    unsafe { PLIC_IRQ_CHIP.as_ref().unwrap().clone() }
}

#[inline(always)]
fn plic_handlers() -> &'static PerCpuVar<PlicHandler> {
    unsafe { PLIC_HANDLERS.as_ref().unwrap() }
}

#[allow(dead_code)]
struct PlicChipData {
    irq_domain: Weak<IrqDomain>,
    phandle: u32,
    lmask: SpinLock<CpuMask>,
    mmio_guard: Option<MMIOSpaceGuard>,
    regs: VirtAddr,
}

impl PlicChipData {
    fn new(
        irq_domain: Weak<IrqDomain>,
        mmio_guard: MMIOSpaceGuard,
        regs: VirtAddr,
        phandle: u32,
    ) -> Arc<Self> {
        let r = Self {
            irq_domain,
            lmask: SpinLock::new(CpuMask::new()),
            mmio_guard: Some(mmio_guard),
            regs,
            phandle,
        };

        Arc::new(r)
    }

    fn lmask(&self) -> SpinLockGuard<CpuMask> {
        self.lmask.lock()
    }
}

impl Debug for PlicChipData {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PlicChipData").finish()
    }
}

impl IrqChipData for PlicChipData {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }
}

struct PlicHandler {
    priv_data: Option<Arc<PlicChipData>>,
    present: AtomicBool,
    inner: SpinLock<InnerPlicHandler>,
}

struct InnerPlicHandler {
    hart_base: VirtAddr,
    enable_base: VirtAddr,
    enable_save: Option<AllocBitmap>,
}

impl PlicHandler {
    fn new() -> Self {
        let inner = InnerPlicHandler {
            hart_base: VirtAddr::new(0),
            enable_base: VirtAddr::new(0),
            enable_save: None,
        };
        Self {
            priv_data: None,
            present: AtomicBool::new(false),
            inner: SpinLock::new(inner),
        }
    }

    fn set_threshold(&self, threshold: u32) {
        let inner = self.inner();
        unsafe {
            /* priority must be > threshold to trigger an interrupt */
            core::ptr::write_volatile(
                (inner.hart_base + PlicIrqChip::CONTEXT_THRESHOLD).data() as *mut u32,
                threshold,
            );
        }
    }

    unsafe fn force_set_priv_data(&mut self, priv_data: Arc<PlicChipData>) {
        self.priv_data = Some(priv_data);
    }

    fn priv_data(&self) -> Option<Arc<PlicChipData>> {
        self.priv_data.clone()
    }

    fn present(&self) -> bool {
        self.present.load(core::sync::atomic::Ordering::SeqCst)
    }

    fn set_present(&self, present: bool) {
        self.present
            .store(present, core::sync::atomic::Ordering::SeqCst);
    }

    fn inner(&self) -> SpinLockGuard<InnerPlicHandler> {
        self.inner.lock()
    }

    fn toggle(&self, hwirq: HardwareIrqNumber, enable: bool) {
        let inner = self.inner();
        let reg = (inner.enable_base + ((hwirq.data() / 32) * 4) as usize).data() as *mut u32;
        let hwirq_mask = 1 << (hwirq.data() % 32);

        if enable {
            unsafe {
                core::ptr::write_volatile(reg, core::ptr::read_volatile(reg) | hwirq_mask);
            }
        } else {
            unsafe {
                core::ptr::write_volatile(reg, core::ptr::read_volatile(reg) & !hwirq_mask);
            }
        }
    }
}

fn plic_irq_toggle(cpumask: &CpuMask, irq_data: &Arc<IrqData>, enable: bool) {
    cpumask.iter_cpu().for_each(|cpu| {
        debug!("plic: irq_toggle: cpu: {cpu:?}");
        let handler = unsafe { plic_handlers().force_get(cpu) };
        handler.toggle(irq_data.hardware_irq(), enable);
    });
}

/// SiFive PLIC中断控制器
///
/// https://code.dragonos.org.cn/xref/linux-6.6.21/drivers/irqchip/irq-sifive-plic.c#204
#[derive(Debug)]
struct PlicIrqChip;
#[allow(dead_code)]
impl PlicIrqChip {
    const COMPATIBLE: &'static str = "sifive,plic-1.0.0";

    const MAX_DEVICES: u32 = 1024;
    const MAX_CONTEXTS: u32 = 15872;

    /*
     * Each interrupt source has a priority register associated with it.
     * We always hardwire it to one in Linux.
     */
    const PRIORITY_BASE: usize = 0;
    const PRIORITY_PER_ID: usize = 4;

    /*
     * Each hart context has a vector of interrupt enable bits associated with it.
     * There's one bit for each interrupt source.
     */
    const CONTEXT_ENABLE_BASE: usize = 0x2080;
    const CONTEXT_ENABLE_SIZE: usize = 0x100;

    /*
     * Each hart context has a set of control registers associated with it.  Right
     * now there's only two: a source priority threshold over which the hart will
     * take an interrupt, and a register to claim interrupts.
     */
    const CONTEXT_BASE: usize = 0x201000;
    const CONTEXT_SIZE: usize = 0x2000;
    const CONTEXT_THRESHOLD: usize = 0x00;
    const CONTEXT_CLAIM: usize = 0x04;

    const PLIC_DISABLE_THRESHOLD: u32 = 0x7;
    const PLIC_ENABLE_THRESHOLD: u32 = 0;

    const PLIC_QUIRK_EDGE_INTERRUPT: u32 = 0;
}

impl IrqChip for PlicIrqChip {
    fn name(&self) -> &'static str {
        "SiFive PLIC"
    }
    fn irq_enable(&self, irq_data: &Arc<IrqData>) -> Result<(), SystemError> {
        // warn!("plic: irq_enable");
        let common_data = irq_data.common_data();
        let inner_guard = common_data.inner();
        let mask = inner_guard.effective_affinity();

        plic_irq_toggle(mask, irq_data, true);
        self.irq_unmask(irq_data).expect("irq_unmask failed");

        Ok(())
    }

    fn irq_unmask(&self, irq_data: &Arc<IrqData>) -> Result<(), SystemError> {
        // warn!("plic: irq_unmask");

        let chip_data = irq_data
            .chip_info_read_irqsave()
            .chip_data()
            .ok_or(SystemError::EINVAL)?;
        let plic_chip_data = chip_data
            .as_any_ref()
            .downcast_ref::<PlicChipData>()
            .ok_or(SystemError::EINVAL)?;

        unsafe {
            core::ptr::write_volatile(
                (plic_chip_data.regs
                    + PlicIrqChip::PRIORITY_BASE
                    + irq_data.hardware_irq().data() as usize * PlicIrqChip::PRIORITY_PER_ID)
                    .data() as *mut u32,
                1,
            );
        }

        Ok(())
    }

    fn irq_mask(&self, irq_data: &Arc<IrqData>) -> Result<(), SystemError> {
        let chip_data = irq_data
            .chip_info_read_irqsave()
            .chip_data()
            .ok_or(SystemError::EINVAL)?;
        let plic_chip_data = chip_data
            .as_any_ref()
            .downcast_ref::<PlicChipData>()
            .ok_or(SystemError::EINVAL)?;

        unsafe {
            core::ptr::write_volatile(
                (plic_chip_data.regs
                    + PlicIrqChip::PRIORITY_BASE
                    + irq_data.hardware_irq().data() as usize * PlicIrqChip::PRIORITY_PER_ID)
                    .data() as *mut u32,
                0,
            );
        }

        Ok(())
    }

    fn irq_disable(&self, irq_data: &Arc<IrqData>) {
        debug!("plic: irq_disable");
        let common_data = irq_data.common_data();
        let inner_guard = common_data.inner();
        let mask = inner_guard.effective_affinity();
        plic_irq_toggle(mask, irq_data, false);
    }

    fn irq_eoi(&self, irq_data: &Arc<IrqData>) {
        let handler = plic_handlers().get();

        if core::intrinsics::unlikely(irq_data.common_data().disabled()) {
            handler.toggle(irq_data.hardware_irq(), true);
            unsafe {
                write_volatile(
                    (handler.inner().hart_base + PlicIrqChip::CONTEXT_CLAIM).data() as *mut u32,
                    irq_data.hardware_irq().data(),
                );
            }

            handler.toggle(irq_data.hardware_irq(), false);
        } else {
            // debug!("plic: irq_eoi: hwirq: {:?}", irq_data.hardware_irq());
            unsafe {
                write_volatile(
                    (handler.inner().hart_base + PlicIrqChip::CONTEXT_CLAIM).data() as *mut u32,
                    irq_data.hardware_irq().data(),
                );
            }
        }
    }

    fn irq_ack(&self, _irq: &Arc<IrqData>) {
        todo!()
    }

    fn can_mask_ack(&self) -> bool {
        false
    }

    fn can_set_affinity(&self) -> bool {
        true
    }

    /// 设置中断的亲和性
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/drivers/irqchip/irq-sifive-plic.c#161
    fn irq_set_affinity(
        &self,
        irq_data: &Arc<IrqData>,
        mask_val: &CpuMask,
        force: bool,
    ) -> Result<IrqChipSetMaskResult, SystemError> {
        let chip_data = irq_data
            .chip_info_read_irqsave()
            .chip_data()
            .ok_or(SystemError::EINVAL)?;
        let plic_chip_data = chip_data
            .as_any_ref()
            .downcast_ref::<PlicChipData>()
            .ok_or(SystemError::EINVAL)?;

        let mut amask = plic_chip_data.lmask().deref() & mask_val;
        let cpu = if force {
            mask_val.first()
        } else {
            amask.bitand_assign(smp_cpu_manager().possible_cpus());
            // todo: 随机选择一个CPU
            amask.first()
        }
        .ok_or(SystemError::EINVAL)?;

        if cpu.data() > smp_cpu_manager().present_cpus_count() {
            return Err(SystemError::EINVAL);
        }

        self.irq_disable(irq_data);
        irq_data
            .common_data()
            .set_effective_affinity(CpuMask::from_cpu(cpu));
        if !irq_data.common_data().disabled() {
            self.irq_enable(irq_data).ok();
        }

        Ok(IrqChipSetMaskResult::Done)
    }

    fn can_set_flow_type(&self) -> bool {
        false
    }

    fn flags(&self) -> IrqChipFlags {
        IrqChipFlags::empty()
    }
}

#[inline(never)]
pub fn riscv_sifive_plic_init() -> Result<(), SystemError> {
    static INIT_PLIC_IRQ_CHIP_ONCE: Once = Once::new();
    INIT_PLIC_IRQ_CHIP_ONCE.call_once(|| unsafe {
        PLIC_IRQ_CHIP = Some(Arc::new(PlicIrqChip));

        PLIC_HANDLERS = Some(
            PerCpuVar::new(
                (0..PerCpu::MAX_CPU_NUM)
                    .map(|_| PlicHandler::new())
                    .collect(),
            )
            .unwrap(),
        );
    });

    let fdt = open_firmware_fdt_driver().fdt_ref()?;
    let all_plics = fdt.all_nodes().filter(|x| {
        if let Some(compatible) = x.compatible() {
            compatible
                .all()
                .any(|x| x == PlicIrqChip::COMPATIBLE || x == "riscv,plic0")
        } else {
            false
        }
    });
    for node in all_plics {
        if let Err(e) = do_riscv_sifive_plic_init(&node) {
            warn!("Failed to init SiFive PLIC: node: {node:?} {e:?}");
        }
    }

    unsafe { riscv::register::sie::set_sext() };
    Ok(())
}

/// 初始化SiFive PLIC
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/drivers/irqchip/irq-sifive-plic.c#415
fn do_riscv_sifive_plic_init(fdt_node: &FdtNode) -> Result<(), SystemError> {
    let reg = fdt_node
        .reg()
        .ok_or(SystemError::EINVAL)?
        .next()
        .ok_or(SystemError::EIO)?;
    let paddr = PhysAddr::new(reg.starting_address as usize);
    let size = reg.size.ok_or(SystemError::EINVAL)?;
    let mmio_guard = mmio_pool().create_mmio(size)?;
    let vaddr = unsafe { mmio_guard.map_any_phys(paddr, size) }?;

    let phandle = fdt_node
        .property("phandle")
        .ok_or(SystemError::EINVAL)?
        .as_usize()
        .ok_or(SystemError::EINVAL)?;

    // 中断数量
    let irq_num = fdt_node
        .property("riscv,ndev")
        .ok_or(SystemError::EINVAL)?
        .as_usize()
        .ok_or(SystemError::EINVAL)?;
    debug!(
        "plic: node: {}, irq_num: {irq_num}, paddr: {paddr:?}, size: {size}",
        fdt_node.name
    );
    let nr_contexts = fdt_node
        .interrupts_extended()
        .ok_or(SystemError::EINVAL)?
        .count();
    debug!("plic: nr_contexts: {nr_contexts}");

    let irq_domain = irq_domain_manager()
        .create_and_add_linear(
            fdt_node.name.to_string(),
            &PlicIrqDomainOps,
            (irq_num + 1) as u32,
        )
        .ok_or(SystemError::EINVAL)?;
    // debug!("plic: irq_domain: {irq_domain:?}");

    let priv_data = PlicChipData::new(
        Arc::downgrade(&irq_domain),
        mmio_guard,
        vaddr,
        phandle as u32,
    );
    irq_domain.set_host_data(Some(priv_data.clone() as Arc<dyn IrqChipData>));

    let loop_done_setup = |irq_handler: &PlicHandler| {
        for x in 1..=irq_num {
            irq_handler.toggle(HardwareIrqNumber::new(x as u32), false);

            unsafe {
                core::ptr::write_volatile(
                    (priv_data.regs + PlicIrqChip::PRIORITY_BASE + x * PlicIrqChip::PRIORITY_PER_ID)
                        .data() as *mut u32,
                    1,
                )
            };
        }
    };

    // todo: 学习linux那样处理，获取到hartid，这里暂时糊代码
    // linux: https://code.dragonos.org.cn/xref/linux-6.6.21/drivers/irqchip/irq-sifive-plic.c#458
    for i in smp_cpu_manager().present_cpus().iter_cpu() {
        let i = i.data() as usize;

        let cpu = ProcessorId::new(i as u32);
        let handler = unsafe { plic_handlers().force_get(cpu) };
        if handler.present() {
            warn!("plic: handler {i} already present.");
            handler.set_threshold(PlicIrqChip::PLIC_ENABLE_THRESHOLD);
            loop_done_setup(handler);
            continue;
        }

        debug!("plic: setup lmask {cpu:?}.");
        priv_data.lmask().set(cpu, true);
        let mut handler_inner = handler.inner();
        handler_inner.hart_base =
            priv_data.regs + PlicIrqChip::CONTEXT_BASE + i * PlicIrqChip::CONTEXT_SIZE;
        handler_inner.enable_base = priv_data.regs
            + PlicIrqChip::CONTEXT_ENABLE_BASE
            + i * PlicIrqChip::CONTEXT_ENABLE_SIZE;
        handler.set_present(true);
        unsafe {
            plic_handlers()
                .force_get_mut(cpu)
                .force_set_priv_data(priv_data.clone())
        };

        handler_inner.enable_save = Some(AllocBitmap::new(irq_num as usize));

        drop(handler_inner);
        handler.set_threshold(PlicIrqChip::PLIC_ENABLE_THRESHOLD);

        loop_done_setup(handler);
    }

    // 把外部设备的中断与PLIC关联起来
    associate_irq_with_plic_domain(&irq_domain, phandle as u32).ok();

    Ok(())
}

/// 把设备的中断与PLIC的关联起来
fn associate_irq_with_plic_domain(
    irq_domain: &Arc<IrqDomain>,
    plic_phandle: u32,
) -> Result<(), SystemError> {
    let fdt_ref = open_firmware_fdt_driver().fdt_ref()?;
    let nodes = fdt_ref.all_nodes().filter(|x| {
        if let Some(pa) = x.property("interrupt-parent").and_then(|x| x.as_usize()) {
            pa as u32 == plic_phandle
        } else {
            false
        }
    });

    for node in nodes {
        if let Some(irq) = node.interrupts().and_then(|mut x| x.next()) {
            let irq = irq as u32;
            let virq = IrqNumber::new(irq);
            let hwirq = HardwareIrqNumber::new(irq);
            debug!("plic: associate irq: {irq}, virq: {virq:?}, hwirq: {hwirq:?}");
            irq_domain_manager()
                .domain_associate(irq_domain, virq, hwirq)
                .ok();
        }
    }

    Ok(())
}
#[derive(Debug)]
struct PlicIrqDomainOps;

impl IrqDomainOps for PlicIrqDomainOps {
    fn unmap(&self, _irq_domain: &Arc<IrqDomain>, _virq: IrqNumber) {
        todo!()
    }

    fn map(
        &self,
        irq_domain: &Arc<IrqDomain>,
        hwirq: HardwareIrqNumber,
        virq: IrqNumber,
    ) -> Result<(), SystemError> {
        // debug!("plic: map: virq: {virq:?}, hwirq: {hwirq:?}");

        let chip_data = irq_domain.host_data().ok_or(SystemError::EINVAL)?;
        let plic_chip_data = chip_data
            .as_any_ref()
            .downcast_ref::<PlicChipData>()
            .ok_or(SystemError::EINVAL)?;
        irq_domain_manager().domain_set_info(
            irq_domain,
            virq,
            hwirq,
            plic_irq_chip(),
            irq_domain.host_data(),
            fast_eoi_irq_handler(),
            None,
            None,
        );
        let irq_desc = irq_desc_manager().lookup(virq).unwrap();
        irq_desc.set_noprobe();
        let mask = plic_chip_data.lmask().clone();
        irq_manager().irq_set_affinity(&irq_desc.irq_data(), &irq_desc.inner(), &mask)?;
        Ok(())
    }

    fn activate(
        &self,
        _domain: &Arc<IrqDomain>,
        _irq_data: &Arc<IrqData>,
        _reserve: bool,
    ) -> Result<(), SystemError> {
        warn!("plic: activate");
        loop {}
    }

    fn deactivate(&self, _domain: &Arc<IrqDomain>, _irq_data: &Arc<IrqData>) {}
}

/// 处理PLIC中断
pub(super) fn do_plic_irq(trap_frame: &mut TrapFrame) {
    // debug!("plic: do_plic_irq");

    let handler = plic_handlers().get();
    let priv_data = handler.priv_data();
    if priv_data.is_none() {
        return;
    }

    let domain = priv_data.unwrap().irq_domain.upgrade();
    if domain.is_none() {
        return;
    }

    let domain = domain.unwrap();

    // 循环处理中断
    loop {
        let claim = unsafe {
            read_volatile(
                (handler.inner().hart_base + PlicIrqChip::CONTEXT_CLAIM).data() as *const u32,
            )
        };

        if claim == 0 {
            break;
        }
        // debug!("plic: claim: {claim:?}");

        let hwirq = HardwareIrqNumber::new(claim);
        if let Err(e) = GenericIrqHandler::handle_domain_irq(domain.clone(), hwirq, trap_frame) {
            warn!("plic: can't find mapping for hwirq {hwirq:?}, {e:?}");
        }
    }
}
