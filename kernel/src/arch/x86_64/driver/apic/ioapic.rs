use core::ptr::NonNull;

use acpi::madt::Madt;
use bit_field::BitField;
use bitflags::bitflags;

use crate::{
    driver::acpi::acpi_manager,
    kdebug, kinfo,
    libs::{
        once::Once,
        spinlock::SpinLock,
        volatile::{volwrite, Volatile},
    },
    mm::{
        mmio_buddy::{mmio_pool, MMIOSpaceGuard},
        PhysAddr,
    },
    syscall::SystemError,
};

use super::{CurrentApic, LocalAPIC};

static mut __IOAPIC: Option<SpinLock<IoApic>> = None;

#[allow(non_snake_case)]
fn IOAPIC() -> &'static SpinLock<IoApic> {
    unsafe { __IOAPIC.as_ref().unwrap() }
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

    /// 边沿响应
    #[allow(dead_code)]
    fn edge_ack(&mut self, _irq_num: u8) {
        CurrentApic.send_eoi();
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

pub fn ioapic_init() {
    kinfo!("Initializing ioapic...");
    let ioapic = unsafe { IoApic::new() };
    unsafe {
        __IOAPIC = Some(SpinLock::new(ioapic));
    }
    kinfo!("IO Apic initialized.");
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
pub(super) fn ioapic_install(
    vector: u8,
    dest: u8,
    level_triggered: bool,
    active_high: bool,
    dest_logic: bool,
) -> Result<(), SystemError> {
    let rte_index = IoApic::vector_rte_index(vector);
    return IOAPIC().lock_irqsave().install(
        rte_index,
        vector,
        dest,
        level_triggered,
        active_high,
        dest_logic,
        true,
    );
}

/// 卸载中断
pub(super) fn ioapic_uninstall(vector: u8) {
    let rte_index = IoApic::vector_rte_index(vector);
    IOAPIC().lock_irqsave().disable(rte_index);
}

/// 使能中断
pub(super) fn ioapic_enable(vector: u8) {
    let rte_index = IoApic::vector_rte_index(vector);
    IOAPIC().lock_irqsave().enable(rte_index);
}

/// 禁用中断
pub(super) fn ioapic_disable(vector: u8) {
    let rte_index = IoApic::vector_rte_index(vector);
    IOAPIC().lock_irqsave().disable(rte_index);
}
