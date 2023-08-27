use bit_field::BitField;
use bitflags::bitflags;

#[allow(dead_code)]
pub struct IoApic {
    reg: *mut u32,
    data: *mut u32,
}

impl IoApic {
    /// Create a new IOAPIC.
    ///
    /// # Safety
    ///
    /// You must provide a valid address.
    pub unsafe fn new(addr: usize) -> Self {
        IoApic {
            reg: addr as *mut u32,
            data: (addr + 0x10) as *mut u32,
        }
    }

    pub fn disable_all(&mut self) {
        // Mark all interrupts edge-triggered, active high, disabled,
        // and not routed to any CPUs.
        for i in 0..=self.maxintr() {
            self.disable(i);
        }
    }

    unsafe fn read(&mut self, reg: u8) -> u32 {
        assert!(!(0x3..REG_TABLE).contains(&reg));
        self.reg.write_volatile(reg as u32);
        self.data.read_volatile()
    }

    unsafe fn write(&mut self, reg: u8, data: u32) {
        // 0x1 & 0x2 are read-only regs
        assert!(!(0x1..REG_TABLE).contains(&reg));
        self.reg.write_volatile(reg as u32);
        self.data.write_volatile(data);
    }

    fn write_irq(&mut self, irq: u8, vector: u8, flags: RedirectionEntry, dest: u8) {
        unsafe {
            self.write(REG_TABLE + 2 * irq, vector as u32 | flags.bits());
            self.write(REG_TABLE + 2 * irq + 1, (dest as u32) << 24);
        }
    }

    pub fn enable(&mut self, irq: u8, cpunum: u8) {
        // 标记中断边沿触发、高电平有效、
        // 启用并路由到给定的 cpunum，即是是该 cpu 的 APIC ID（不是cpuid）
        let vector = self.irq_vector(irq);
        self.write_irq(irq, vector, RedirectionEntry::NONE, cpunum);
    }

    pub fn disable(&mut self, irq: u8) {
        let vector = self.irq_vector(irq);
        self.write_irq(irq, vector, RedirectionEntry::DISABLED, 0);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn config(
        &mut self,
        irq_offset: u8,
        vector: u8,
        dest: u8,
        level_triggered: bool,
        active_high: bool,
        dest_logic: bool,
        mut mask: bool,
    ) {
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
        self.write_irq(irq_offset, vector, flags, dest)
    }

    pub fn irq_vector(&mut self, irq: u8) -> u8 {
        unsafe { self.read(REG_TABLE + 2 * irq).get_bits(0..8) as u8 }
    }

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

    pub fn id(&mut self) -> u8 {
        unsafe { self.read(REG_ID).get_bits(24..28) as u8 }
    }
    // ics的类型
    pub fn version(&mut self) -> u8 {
        unsafe { self.read(REG_VER).get_bits(0..8) as u8 }
    }
    // 获取range指定的位范围； 注意，索引 0 是最低有效位，而索引 length() - 1 是最高有效位。元素总个数
    pub fn maxintr(&mut self) -> u8 {
        unsafe { self.read(REG_VER).get_bits(16..24) as u8 }
    }
}

/// Default physical address of IO APIC
/// 设置IO APIC ID 为0x0f000000
/// *apic_ioapic_map.virtual_index_addr = 0x00;
/// io_mfence();
/// *apic_ioapic_map.virtual_data_addr = 0x0f000000;
/// io_mfence();
pub const IOAPIC_ADDR: u32 = 0x0f000000;
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
