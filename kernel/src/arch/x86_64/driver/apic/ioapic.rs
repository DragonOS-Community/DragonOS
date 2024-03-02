use core::ptr::NonNull;

use acpi::madt::Madt;
use alloc::sync::Arc;
use bit_field::BitField;
use bitflags::bitflags;
use system_error::SystemError;

use crate::{
    driver::acpi::acpi_manager,
    exception::{
        handle::{edge_irq_handler, fast_eoi_irq_handler},
        irqchip::{IrqChip, IrqChipData, IrqChipFlags, IrqChipSetMaskResult, IrqChipState},
        irqdata::{IrqData, IrqLineStatus},
        irqdesc::{irq_desc_manager, IrqDesc, IrqFlowHandler},
        manage::irq_manager,
        IrqNumber,
    },
    kdebug, kinfo,
    libs::{
        cpumask::CpuMask,
        once::Once,
        spinlock::{SpinLock, SpinLockGuard},
        volatile::{volwrite, Volatile},
    },
    mm::{
        mmio_buddy::{mmio_pool, MMIOSpaceGuard},
        PhysAddr,
    },
};

use super::{CurrentApic, LocalAPIC};

static mut __IOAPIC: Option<SpinLock<IoApic>> = None;
static mut IOAPIC_IR_CHIP: Option<Arc<IoApicChip>> = None;

#[allow(non_snake_case)]
fn IOAPIC() -> &'static SpinLock<IoApic> {
    unsafe { __IOAPIC.as_ref().unwrap() }
}

#[inline(always)]
pub(super) fn ioapic_ir_chip() -> Arc<dyn IrqChip> {
    unsafe { IOAPIC_IR_CHIP.as_ref().unwrap().clone() }
}

#[allow(dead_code)]
pub struct IoApic {
    reg: *mut u32,
    data: *mut u32,
    virt_eoi: *mut u32,
    phys_base: PhysAddr,
    mmio_guard: MMIOSpaceGuard,
}

impl IoApic {
    /// IO APIC的中断向量号从32开始
    pub const VECTOR_BASE: u8 = 32;

    /// Create a new IOAPIC.
    ///
    /// # Safety
    ///
    /// You must provide a valid address.
    pub unsafe fn new() -> Self {
        static INIT_STATE: Once = Once::new();
        assert!(!INIT_STATE.is_completed());

        let mut result: Option<IoApic> = None;
        INIT_STATE.call_once(|| {
            kinfo!("Initializing ioapic...");

            // get ioapic base from acpi

            let madt = acpi_manager()
                .tables()
                .unwrap()
                .find_table::<Madt>()
                .expect("IoApic::new(): failed to find MADT");

            let io_apic_paddr = madt
                .entries()
                .find(|x| {
                    if let acpi::madt::MadtEntry::IoApic(_x) = x {
                        return true;
                    }
                    return false;
                })
                .map(|x| {
                    if let acpi::madt::MadtEntry::IoApic(x) = x {
                        Some(x.io_apic_address)
                    } else {
                        None
                    }
                })
                .flatten()
                .unwrap();

            let phys_base = PhysAddr::new(io_apic_paddr as usize);

            let mmio_guard = mmio_pool()
                .create_mmio(0x1000)
                .expect("IoApic::new(): failed to create mmio");
            assert!(
                mmio_guard.map_phys(phys_base, 0x1000).is_ok(),
                "IoApic::new(): failed to map phys"
            );
            kdebug!("Ioapic map ok");
            let reg = mmio_guard.vaddr();

            result = Some(IoApic {
                reg: reg.data() as *mut u32,
                data: (reg + 0x10).data() as *mut u32,
                virt_eoi: (reg + 0x40).data() as *mut u32,
                phys_base,
                mmio_guard,
            });
            kdebug!("IOAPIC: to mask all RTE");
            // 屏蔽所有的RTE
            let res_mut = result.as_mut().unwrap();
            for i in 0..res_mut.supported_interrupts() {
                res_mut.write_rte(i, 0x20 + i, RedirectionEntry::DISABLED, 0);
            }
            kdebug!("Ioapic init done");
        });

        assert!(
            result.is_some(),
            "Failed to init ioapic, maybe this is a double initialization bug?"
        );
        return result.unwrap();
    }

    /// Disable all interrupts.
    #[allow(dead_code)]
    pub fn disable_all(&mut self) {
        // Mark all interrupts edge-triggered, active high, disabled,
        // and not routed to any CPUs.
        for i in 0..self.supported_interrupts() {
            self.disable(i);
        }
    }

    unsafe fn read(&mut self, reg: u8) -> u32 {
        assert!(!(0x3..REG_TABLE).contains(&reg));
        self.reg.write_volatile(reg as u32);
        self.data.read_volatile()
    }

    /// 直接写入REG_TABLE内的寄存器
    ///
    /// ## 参数
    ///
    /// * `reg` - 寄存器下标
    /// * `data` - 寄存器数据
    unsafe fn write(&mut self, reg: u8, data: u32) {
        // 0x1 & 0x2 are read-only regs
        assert!(!(0x1..REG_TABLE).contains(&reg));
        self.reg.write_volatile(reg as u32);
        self.data.write_volatile(data);
    }

    fn write_rte(&mut self, rte_index: u8, vector: u8, flags: RedirectionEntry, dest: u8) {
        unsafe {
            self.write(REG_TABLE + 2 * rte_index, vector as u32 | flags.bits());
            self.write(REG_TABLE + 2 * rte_index + 1, (dest as u32) << 24);
        }
    }

    /// 标记中断边沿触发、高电平有效、
    /// 启用并路由到给定的 cpunum，即是是该 cpu 的 APIC ID（不是cpuid）
    pub fn enable(&mut self, rte_index: u8) {
        let mut val = unsafe { self.read(REG_TABLE + 2 * rte_index) };
        val &= !RedirectionEntry::DISABLED.bits();
        unsafe { self.write(REG_TABLE + 2 * rte_index, val) };
    }

    pub fn disable(&mut self, rte_index: u8) {
        let reg = REG_TABLE + 2 * rte_index;
        let mut val = unsafe { self.read(reg) };
        val |= RedirectionEntry::DISABLED.bits();
        unsafe { self.write(reg, val) };
    }

    /// 安装中断
    ///
    /// ## 参数
    ///
    /// * `rte_index` - RTE下标
    /// * `vector` - 中断向量号
    /// * `dest` - 目标CPU的APIC ID
    /// * `level_triggered` - 是否为电平触发
    /// * `active_high` - 是否为高电平有效
    /// * `dest_logic` - 是否为逻辑模式
    /// * `mask` - 是否屏蔽
    pub fn install(
        &mut self,
        rte_index: u8,
        vector: u8,
        dest: u8,
        level_triggered: bool,
        active_high: bool,
        dest_logic: bool,
        mut mask: bool,
    ) -> Result<(), SystemError> {
        // 重定向表从 REG_TABLE 开始，使用两个寄存器来配置每个中断。
        // 一对中的第一个（低位）寄存器包含配置位。32bit
        // 第二个（高）寄存器包含一个位掩码，告诉哪些 CPU 可以服务该中断。
        //  level_triggered：如果为真，表示中断触发方式为电平触发（level-triggered），则将RedirectionEntry::LEVEL标志位设置在flags中。
        //  active_high：如果为假，表示中断的极性为低电平有效（active-low），则将RedirectionEntry::ACTIVELOW标志位设置在flags中。
        //  dest_logic：如果为真，表示中断目标为逻辑模式（logical mode），则将RedirectionEntry::LOGICAL标志位设置在flags中。
        //  !(0x20..=0xef).contains(&vector)：判断中断向量号（vector）是否在范围0x20到0xef之外，如果是，则表示中断无效，将mask标志位设置为真。
        //  mask：如果为真，表示中断被屏蔽（masked），将RedirectionEntry::DISABLED标志位设置在flags中。
        let mut flags = RedirectionEntry::NONE;
        if level_triggered {
            flags |= RedirectionEntry::LEVEL;
        }
        if !active_high {
            flags |= RedirectionEntry::ACTIVELOW;
        }
        if dest_logic {
            flags |= RedirectionEntry::LOGICAL;
        }
        if !(0x20..=0xef).contains(&vector) {
            mask = true;
        }
        if mask {
            flags |= RedirectionEntry::DISABLED;
        }
        self.write_rte(rte_index, vector, flags, dest);
        return Ok(());
    }

    /// Get the vector number for the given IRQ.
    #[allow(dead_code)]
    pub fn irq_vector(&mut self, irq: u8) -> u8 {
        unsafe { self.read(REG_TABLE + 2 * irq).get_bits(0..8) as u8 }
    }

    /// Set the vector number for the given IRQ.
    #[allow(dead_code)]
    pub fn set_irq_vector(&mut self, irq: u8, vector: u8) {
        let mut old = unsafe { self.read(REG_TABLE + 2 * irq) };
        let old_vector = old.get_bits(0..8);
        if !(0x20..=0xfe).contains(&old_vector) {
            old |= RedirectionEntry::DISABLED.bits();
        }
        unsafe {
            self.write(REG_TABLE + 2 * irq, *old.set_bits(0..8, vector as u32));
        }
    }

    #[allow(dead_code)]
    pub fn id(&mut self) -> u8 {
        unsafe { self.read(REG_ID).get_bits(24..28) as u8 }
    }

    /// IO APIC Version
    #[allow(dead_code)]
    pub fn version(&mut self) -> u8 {
        unsafe { self.read(REG_VER).get_bits(0..8) as u8 }
    }

    /// Number of supported interrupts by this IO APIC.
    ///
    /// Max Redirection Entry = "how many IRQs can this I/O APIC handle - 1"
    /// The -1 is silly so we add one back to it.
    pub fn supported_interrupts(&mut self) -> u8 {
        unsafe { (self.read(REG_VER).get_bits(16..24) + 1) as u8 }
    }

    pub fn pending(&mut self, irq: u8) -> bool {
        let rte_index = Self::vector_rte_index(irq);
        let data = unsafe { self.read(REG_TABLE + 2 * rte_index) };
        data & (1 << 12) != 0
    }

    fn vector_rte_index(irq_num: u8) -> u8 {
        assert!(irq_num >= Self::VECTOR_BASE);
        irq_num - Self::VECTOR_BASE
    }

    /// 电平响应
    #[allow(dead_code)]
    fn level_ack(&mut self, irq_num: u8) {
        #[repr(C)]
        struct LevelAck {
            virt_eoi: Volatile<u32>,
        }

        let p = NonNull::new(self.virt_eoi as *mut LevelAck).unwrap();

        unsafe {
            volwrite!(p, virt_eoi, irq_num as u32);
        }
    }
}

/// Register index: ID
const REG_ID: u8 = 0x00;
/// 获取IO APIC Version
const REG_VER: u8 = 0x01;
/// Redirection table base
const REG_TABLE: u8 = 0x10;

bitflags! {
    /// The redirection table starts at REG_TABLE and uses
    /// two registers to configure each interrupt.
    /// The first (low) register in a pair contains configuration bits.
    /// The second (high) register contains a bitmask telling which
    /// CPUs can serve that interrupt.
    struct RedirectionEntry: u32 {
        /// Interrupt disabled
        const DISABLED  = 0x00010000;
        /// Level-triggered (vs edge-)
        const LEVEL     = 0x00008000;
        /// Active low (vs high)
        const ACTIVELOW = 0x00002000;
        /// Destination is CPU id (vs APIC ID)
        const LOGICAL   = 0x00000800;
        /// None
        const NONE		= 0x00000000;
    }
}

#[derive(Debug)]
struct IoApicChipData {
    inner: SpinLock<InnerIoApicChipData>,
}

impl IrqChipData for IoApicChipData {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }
}

impl IoApicChipData {
    const DEFAULT: Self = Self::new(0, 0, 0, false, false, false, true);

    const fn new(
        rte_index: u8,
        vector: u8,
        dest: u8,
        level_triggered: bool,
        active_high: bool,
        dest_logic: bool,
        mask: bool,
    ) -> Self {
        IoApicChipData {
            inner: SpinLock::new(InnerIoApicChipData {
                rte_index,
                vector,
                dest,
                level_triggered,
                active_high,
                dest_logic,
                mask,
            }),
        }
    }

    fn inner(&self) -> SpinLockGuard<InnerIoApicChipData> {
        self.inner.lock_irqsave()
    }
}

#[derive(Debug)]
struct InnerIoApicChipData {
    rte_index: u8,
    vector: u8,
    dest: u8,
    level_triggered: bool,
    active_high: bool,
    dest_logic: bool,
    mask: bool,
}

impl InnerIoApicChipData {
    /// 把中断数据同步到芯片
    fn sync_to_chip(&self) -> Result<(), SystemError> {
        ioapic_install(
            self.vector,
            self.dest,
            self.level_triggered,
            self.active_high,
            self.dest_logic,
            self.mask,
        )
    }
}

#[inline(never)]
pub fn ioapic_init(ignore: &'static [IrqNumber]) {
    kinfo!("Initializing ioapic...");
    let ioapic = unsafe { IoApic::new() };
    unsafe {
        __IOAPIC = Some(SpinLock::new(ioapic));
    }
    unsafe {
        IOAPIC_IR_CHIP = Some(Arc::new(IoApicChip));
    }

    // 绑定irqchip
    for i in 32..256 {
        let irq = IrqNumber::new(i);

        if ignore.contains(&irq) {
            continue;
        }

        let desc = irq_desc_manager().lookup(irq).unwrap();
        let irq_data = desc.irq_data();
        let mut chip_info_guard = irq_data.chip_info_write_irqsave();
        chip_info_guard.set_chip(Some(ioapic_ir_chip()));
        let chip_data = IoApicChipData::DEFAULT;
        chip_data.inner().rte_index = IoApic::vector_rte_index(i as u8);
        chip_data.inner().vector = i as u8;
        chip_info_guard.set_chip_data(Some(Arc::new(chip_data)));
        drop(chip_info_guard);
        let level = irq_data.is_level_type();

        register_handler(&desc, level);
    }

    kinfo!("IO Apic initialized.");
}

fn register_handler(desc: &Arc<IrqDesc>, level_triggered: bool) {
    let fasteoi: bool;
    if level_triggered {
        desc.modify_status(IrqLineStatus::empty(), IrqLineStatus::IRQ_LEVEL);
        fasteoi = true;
    } else {
        desc.modify_status(IrqLineStatus::IRQ_LEVEL, IrqLineStatus::empty());
        fasteoi = false;
    }

    let handler: &dyn IrqFlowHandler = if fasteoi {
        fast_eoi_irq_handler()
    } else {
        edge_irq_handler()
    };
    desc.set_handler(handler);
}

/// 安装中断
///
/// ## 参数
///
/// * `vector` - 中断向量号
/// * `dest` - 目标CPU的APIC ID
/// * `level_triggered` - 是否为电平触发
/// * `active_high` - 是否为高电平有效
/// * `dest_logic` - 是否为逻辑模式
/// * `mask` - 是否屏蔽
fn ioapic_install(
    vector: u8,
    dest: u8,
    level_triggered: bool,
    active_high: bool,
    dest_logic: bool,
    mask: bool,
) -> Result<(), SystemError> {
    let rte_index = IoApic::vector_rte_index(vector);
    return IOAPIC().lock_irqsave().install(
        rte_index,
        vector,
        dest,
        level_triggered,
        active_high,
        dest_logic,
        mask,
    );
}

/// IoApic中断芯片
///
/// https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/kernel/apic/io_apic.c#1994
#[derive(Debug)]
struct IoApicChip;

impl IrqChip for IoApicChip {
    fn name(&self) -> &'static str {
        "IR-IO-APIC"
    }

    fn irq_startup(&self, irq: &Arc<IrqData>) -> Result<(), SystemError> {
        self.irq_unmask(irq)
    }

    fn irq_mask(&self, irq: &Arc<IrqData>) -> Result<(), SystemError> {
        let binding = irq
            .chip_info_read_irqsave()
            .chip_data()
            .ok_or(SystemError::EINVAL)?;
        let chip_data = binding
            .as_any_ref()
            .downcast_ref::<IoApicChipData>()
            .ok_or(SystemError::EINVAL)?;

        let mut chip_data_inner = chip_data.inner();
        chip_data_inner.mask = true;
        chip_data_inner.sync_to_chip().ok();

        drop(chip_data_inner);

        return Ok(());
    }

    fn can_set_affinity(&self) -> bool {
        true
    }

    fn can_set_flow_type(&self) -> bool {
        true
    }

    fn irq_set_type(
        &self,
        irq: &Arc<IrqData>,
        flow_type: IrqLineStatus,
    ) -> Result<IrqChipSetMaskResult, SystemError> {
        let binding = irq
            .chip_info_read_irqsave()
            .chip_data()
            .ok_or(SystemError::EINVAL)?;
        let chip_data = binding
            .as_any_ref()
            .downcast_ref::<IoApicChipData>()
            .ok_or(SystemError::EINVAL)?;
        let mut chip_data_inner = chip_data.inner();

        let level_triggered = flow_type.is_level_type();
        let active_high = flow_type.is_level_high().unwrap_or(false);
        chip_data_inner.active_high = active_high;
        chip_data_inner.level_triggered = level_triggered;
        chip_data_inner.sync_to_chip()?;

        return Ok(IrqChipSetMaskResult::SetMaskOk);
    }

    fn irq_set_affinity(
        &self,
        irq: &Arc<IrqData>,
        cpu: &CpuMask,
        _force: bool,
    ) -> Result<IrqChipSetMaskResult, SystemError> {
        // 使用mask的第1个可用CPU
        let dest = (cpu.first().ok_or(SystemError::EINVAL)?.data() & 0xff) as u8;

        let binding = irq
            .chip_info_read_irqsave()
            .chip_data()
            .ok_or(SystemError::EINVAL)?;
        let chip_data = binding
            .as_any_ref()
            .downcast_ref::<IoApicChipData>()
            .ok_or(SystemError::EINVAL)?;

        let mut chip_data_inner = chip_data.inner();
        let origin_dest = chip_data_inner.dest;
        if origin_dest == dest {
            return Ok(IrqChipSetMaskResult::SetMaskOk);
        }

        chip_data_inner.dest = dest;

        chip_data_inner.sync_to_chip()?;

        return Ok(IrqChipSetMaskResult::SetMaskOk);
    }

    fn irq_unmask(&self, irq: &Arc<IrqData>) -> Result<(), SystemError> {
        IOAPIC()
            .lock_irqsave()
            .enable(IoApic::vector_rte_index(irq.irq().data() as u8));
        Ok(())
    }

    fn can_mask_ack(&self) -> bool {
        true
    }

    fn irq_mask_ack(&self, irq: &Arc<IrqData>) {
        self.irq_mask(irq).ok();
        self.irq_eoi(irq);
    }

    fn irq_eoi(&self, irq: &Arc<IrqData>) {
        if irq.is_level_type() {
            IOAPIC().lock_irqsave().level_ack(irq.irq().data() as u8);
        } else {
            CurrentApic.send_eoi();
        }
    }

    fn retrigger(&self, irq_data: &Arc<IrqData>) -> Result<(), SystemError> {
        irq_manager().irq_chip_retrigger_hierarchy(irq_data)
    }

    fn irqchip_state(&self, irq: &Arc<IrqData>, which: IrqChipState) -> Result<bool, SystemError> {
        let binding = irq
            .chip_info_read_irqsave()
            .chip_data()
            .ok_or(SystemError::EINVAL)?;
        let chip_data = binding
            .as_any_ref()
            .downcast_ref::<IoApicChipData>()
            .ok_or(SystemError::EINVAL)?;

        match which {
            IrqChipState::Pending => {
                return Ok(IOAPIC().lock_irqsave().pending(irq.irq().data() as u8));
            }
            IrqChipState::Active => {
                let chip_data_inner = chip_data.inner();
                return Ok(!chip_data_inner.mask);
            }
            IrqChipState::Masked => {
                let chip_data_inner = chip_data.inner();
                return Ok(chip_data_inner.mask);
            }
            IrqChipState::LineLevel => {
                let chip_data_inner = chip_data.inner();
                return Ok(chip_data_inner.active_high);
            }
            #[allow(unreachable_patterns)]
            _ => {
                return Err(SystemError::EINVAL);
            }
        }
    }

    fn irq_disable(&self, irq: &Arc<IrqData>) {
        let binding = irq
            .chip_info_read_irqsave()
            .chip_data()
            .ok_or(SystemError::EINVAL)
            .unwrap();
        let chip_data = binding
            .as_any_ref()
            .downcast_ref::<IoApicChipData>()
            .ok_or(SystemError::EINVAL)
            .unwrap();
        let mut chip_data_inner = chip_data.inner();
        chip_data_inner.mask = true;
        chip_data_inner.sync_to_chip().ok();
    }

    fn irq_ack(&self, irq_data: &Arc<IrqData>) {
        // irq_manager().irq_chip_ack_parent(irq_data);
        self.irq_eoi(irq_data);
    }

    fn flags(&self) -> IrqChipFlags {
        IrqChipFlags::IRQCHIP_SKIP_SET_WAKE | IrqChipFlags::IRQCHIP_AFFINITY_PRE_STARTUP
    }
}
